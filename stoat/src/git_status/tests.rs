use super::{
    merge_banner, BodyLine, Divergence, FileRow, GitStatusModel, GitStatusPanel, Note, OpBanner,
    OpKind, RebaseProgress, RowKind, Section, SectionKind,
};
use crate::{
    host::{
        git::MergeState, CommitInfo, ConflictedFile, FakeFs, FakeGit, FakeRepoBuilder, FsHost,
        GitHost, LocalFs, LocalGit, RebaseTodoOp,
    },
    rebase::{ActiveRebase, ConflictResolution, RebaseEntry, RebasePause, RebaseState},
    theme::scope,
};
use git2::{Repository, Signature};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use stoat_language::LanguageRegistry;

const ROOT: &str = "/repo";

/// The model of the repo that `seed` builds at [`ROOT`], whose working files
/// sit on a fake filesystem the builder writes.
fn read(
    seed: impl FnOnce(&mut FakeRepoBuilder<'_>),
    rebase: Option<&RebaseProgress>,
    resolved: &HashMap<PathBuf, Note>,
) -> GitStatusModel {
    let host = FakeGit::new();
    let fs = FakeFs::new();
    seed(&mut host.add_repo(ROOT).with_fs(&fs));
    let repo = host.discover(Path::new(ROOT)).expect("the repo is seeded");
    GitStatusModel::read(&*repo, &fs, &LanguageRegistry::deferred(), rebase, resolved)
}

fn row(rel: &str, kind: RowKind, note: Note) -> FileRow {
    FileRow {
        path: Path::new(ROOT).join(rel),
        kind,
        note,
    }
}

fn section(kind: SectionKind, rows: Vec<FileRow>) -> Section {
    Section { kind, rows }
}

fn entry(op: RebaseTodoOp, sha: &str) -> RebaseEntry {
    RebaseEntry {
        op,
        commit: CommitInfo {
            sha: sha.to_string(),
            short_sha: sha.chars().take(7).collect(),
            summary: String::new(),
            author_name: String::new(),
            author_email: String::new(),
            time: 0,
            parents: Vec::new(),
        },
    }
}

/// The board's dirty state. A branch with no upstream measures against local
/// `main`, and each side's files carry that side's line counts, except the
/// untracked file, which carries none. A tracked file gone from disk reads as
/// a deletion.
#[test]
fn a_dirty_repo_lists_each_side_with_its_lines_against_main() {
    let model = read(
        |repo| {
            repo.commit("c1", &[])
                .commit_with_parent("c2", "c1", &[])
                .branch("main", "c1")
                .branch("ermine-tokens", "c2")
                .set_head("c2")
                .set_head_branch("ermine-tokens")
                .head_file("stoat/src/theme.rs", "a")
                .staged_file("stoat/src/theme.rs", "b")
                .lines("stoat/src/theme.rs", 6, 0)
                .modified("stoat/src/render/chrome.rs", "a", "b")
                .lines("stoat/src/render/chrome.rs", 5, 6)
                .deleted("gone.rs", "x")
                .lines("gone.rs", 0, 1)
                .untracked("themes/ermine-night.json")
                .lines("themes/ermine-night.json", 9, 0);
        },
        None,
        &HashMap::new(),
    );

    assert_eq!(
        model,
        GitStatusModel {
            root: PathBuf::from(ROOT),
            branch: Some("ermine-tokens".to_string()),
            divergence: Divergence::AheadBehind(1, 0),
            upstream: "vs main".to_string(),
            op: None,
            sections: vec![
                section(
                    SectionKind::Staged,
                    vec![row(
                        "stoat/src/theme.rs",
                        RowKind::Modified,
                        Note::Lines {
                            added: 6,
                            deleted: 0
                        },
                    )],
                ),
                section(
                    SectionKind::Unstaged,
                    vec![
                        row(
                            "gone.rs",
                            RowKind::Deleted,
                            Note::Lines {
                                added: 0,
                                deleted: 1
                            },
                        ),
                        row(
                            "stoat/src/render/chrome.rs",
                            RowKind::Modified,
                            Note::Lines {
                                added: 5,
                                deleted: 6
                            },
                        ),
                    ],
                ),
                section(
                    SectionKind::Untracked,
                    vec![row(
                        "themes/ermine-night.json",
                        RowKind::Untracked,
                        Note::None
                    )],
                ),
            ],
        },
    );
}

/// The board's merge state. The conflicted file leads with its open chunks,
/// a file the conflict view resolved notes that over its lines, and the
/// banner names the merge and the file left.
#[test]
fn a_merge_in_progress_leads_with_its_banner_and_conflicts() {
    let resolved = HashMap::from([(Path::new(ROOT).join("pool.md"), Note::Resolved)]);
    let model = read(
        |repo| {
            repo.set_head_branch("main")
                .merge_in_progress("f00dfeed1234", Some("smooth-pools"), "Merge")
                .conflicted_file(
                    "pool.rs",
                    Some("a\nb\nc\n"),
                    Some("a\nB\nc\n"),
                    Some("a\nX\nc\n"),
                )
                .head_file("pool.md", "a")
                .staged_file("pool.md", "b")
                .lines("pool.md", 3, 1)
                .head_file("docs.md", "a")
                .staged_file("docs.md", "b");
        },
        None,
        &resolved,
    );

    assert_eq!(
        (model.op, model.sections),
        (
            Some(OpBanner {
                kind: OpKind::Merge,
                head: "smooth-pools \u{2192} main".to_string(),
                message: "2 parents".to_string(),
                detail: "1 file left to resolve".to_string(),
                keys: vec![
                    ("C", "commit", scope::VCS_STATUS_STAGED),
                    ("a", "abort", scope::VCS_STATUS_CONFLICTED),
                ],
            }),
            vec![
                section(
                    SectionKind::Conflicted,
                    vec![row(
                        "pool.rs",
                        RowKind::Unmerged,
                        Note::Open { open: 1, total: 1 }
                    )],
                ),
                section(
                    SectionKind::Staged,
                    vec![
                        row("docs.md", RowKind::Modified, Note::None),
                        row("pool.md", RowKind::Modified, Note::Resolved),
                    ],
                ),
            ],
        ),
    );
}

/// The board's clean state, on a branch level with its upstream.
#[test]
fn a_clean_repo_level_with_its_upstream_has_no_section() {
    let model = read(
        |repo| {
            repo.set_head_branch("main").upstream("origin/main", 0, 0);
        },
        None,
        &HashMap::new(),
    );

    assert_eq!(
        model,
        GitStatusModel {
            root: PathBuf::from(ROOT),
            branch: Some("main".to_string()),
            divergence: Divergence::Equal("origin/main".to_string()),
            upstream: "origin/main".to_string(),
            op: None,
            sections: Vec::new(),
        },
    );
}

/// An upstream wins over `main`, a detached HEAD names its commit, and `main`
/// with no upstream has nothing to measure against.
#[test]
fn the_header_measures_against_the_upstream_or_names_the_commit() {
    let header = |seed: &dyn Fn(&mut FakeRepoBuilder<'_>)| {
        let model = read(|repo| seed(repo), None, &HashMap::new());
        (model.branch, model.divergence, model.upstream)
    };

    assert_eq!(
        [
            header(&|repo| {
                repo.branch("main", "c1")
                    .set_head_branch("feature")
                    .upstream("origin/feature", 7, 2);
            }),
            header(&|repo| {
                repo.commit("c1abcdef99", &[]);
            }),
            header(&|repo| {
                repo.branch("main", "c1").set_head_branch("main");
            }),
        ],
        [
            (
                Some("feature".to_string()),
                Divergence::AheadBehind(7, 2),
                "origin/feature".to_string(),
            ),
            (
                None,
                Divergence::Detached("c1abcde".to_string()),
                String::new()
            ),
            (Some("main".to_string()), Divergence::Unknown, String::new()),
        ],
    );
}

/// A file that both sides of the index change lists under each, as git
/// status lists it, and a deleted file back on disk lists untracked too. Each
/// second row takes its place in path order among the section's rows.
#[test]
fn a_file_changed_on_both_sides_lists_twice() {
    let dir = tempfile::tempdir().expect("create a tempdir");
    let root = dir.path();
    let repo = Repository::init(root).expect("init a repo");
    let write = |name: &str, text: &str| {
        LocalFs
            .write(&root.join(name), text.as_bytes())
            .expect("write");
    };
    let stage = |name: &str| {
        let mut index = repo.index().expect("index");
        index.add_path(Path::new(name)).expect("stage");
        index.write().expect("write the index");
    };
    write("a.rs", "one\n");
    write("both.rs", "one\n");
    write("gone.rs", "one\n");
    stage("a.rs");
    stage("both.rs");
    stage("gone.rs");
    {
        let mut index = repo.index().expect("index");
        let tree = repo
            .find_tree(index.write_tree().expect("write a tree"))
            .expect("find the tree");
        let sig = Signature::now("test", "t@t").expect("sign");
        repo.commit(Some("HEAD"), &sig, &sig, "c", &tree, &[])
            .expect("commit");
    }
    write("a.rs", "two\n");
    write("both.rs", "two\n");
    stage("both.rs");
    write("both.rs", "three\n");
    {
        let mut index = repo.index().expect("index");
        index.remove_path(Path::new("gone.rs")).expect("unstage");
        index.write().expect("write the index");
    }

    let git = LocalGit::new().discover(root).expect("discover the repo");
    let model = GitStatusModel::read(
        &*git,
        &LocalFs,
        &LanguageRegistry::deferred(),
        None,
        &HashMap::new(),
    );
    let at = |rel: &str| root.join(rel);
    let lines = |added, deleted| Note::Lines { added, deleted };
    assert_eq!(
        model.sections,
        [
            section(
                SectionKind::Staged,
                vec![
                    FileRow {
                        path: at("both.rs"),
                        kind: RowKind::Modified,
                        note: lines(1, 1),
                    },
                    FileRow {
                        path: at("gone.rs"),
                        kind: RowKind::Deleted,
                        note: lines(0, 1),
                    },
                ],
            ),
            section(
                SectionKind::Unstaged,
                vec![
                    FileRow {
                        path: at("a.rs"),
                        kind: RowKind::Modified,
                        note: lines(1, 1),
                    },
                    FileRow {
                        path: at("both.rs"),
                        kind: RowKind::Modified,
                        note: lines(1, 1),
                    },
                ],
            ),
            section(
                SectionKind::Untracked,
                vec![FileRow {
                    path: at("gone.rs"),
                    kind: RowKind::Untracked,
                    note: Note::None,
                }],
            ),
        ],
    );
}

/// A rebase that stoat runs counts its progress in the header, names the
/// commit it stacks onto by branch, and names its next entries in the
/// banner.
#[test]
fn a_rebase_counts_its_progress_and_names_its_next_entries() {
    let progress = RebaseProgress {
        done: 3,
        total: 7,
        onto: "c1".to_string(),
        head: "3e9f0aa1234".to_string(),
        message: Some("render: read panel style from theme\n\nThe body.".to_string()),
        next: vec![
            (RebaseTodoOp::Reword, "91bb2c4ff".to_string()),
            (RebaseTodoOp::Squash, "aaa".to_string()),
            (RebaseTodoOp::Drop, "bbb".to_string()),
            (RebaseTodoOp::Edit, "ccc".to_string()),
        ],
        conflicts: Vec::new(),
    };
    let model = read(
        |repo| {
            repo.branch("main", "c1").set_head_branch("ermine-tokens");
        },
        Some(&progress),
        &HashMap::new(),
    );

    assert_eq!(
        (model.divergence, model.op),
        (
            Divergence::Rebase {
                done: 3,
                total: 7,
                onto: "main".to_string(),
            },
            Some(OpBanner {
                kind: OpKind::Rebase,
                head: "3/7 \u{b7} 3e9f0aa".to_string(),
                message: "render: read panel style from theme".to_string(),
                detail: "next: reword 91bb2c4, squash, drop, edit".to_string(),
                keys: vec![
                    ("C", "continue", scope::VCS_STATUS_STAGED),
                    ("s", "skip", scope::UI_DIAGNOSTIC_WARNING),
                    ("a", "abort", scope::VCS_STATUS_CONFLICTED),
                    ("Space r", "plan", scope::VCS_STATUS_STAGED),
                ],
            }),
        ),
    );
}

/// The progress counts the entries applied of the plan's length and names
/// four of the entries left.
#[test]
fn rebase_progress_counts_the_entries_applied() {
    let ops = [
        RebaseTodoOp::Pick,
        RebaseTodoOp::Reword,
        RebaseTodoOp::Squash,
        RebaseTodoOp::Drop,
        RebaseTodoOp::Edit,
        RebaseTodoOp::Fixup,
    ];
    let todo = ops
        .iter()
        .enumerate()
        .map(|(i, op)| entry(*op, &format!("sha{i}")))
        .collect();
    let mut active = ActiveRebase::new(
        RebaseState::new(PathBuf::from(ROOT), "base".to_string(), todo),
        None,
    );
    active.remaining.pop_front();

    let progress = RebaseProgress::of(&active);
    assert_eq!(
        (progress.done, progress.total, progress.next),
        (
            1,
            6,
            vec![
                (RebaseTodoOp::Reword, "sha1".to_string()),
                (RebaseTodoOp::Squash, "sha2".to_string()),
                (RebaseTodoOp::Drop, "sha3".to_string()),
                (RebaseTodoOp::Edit, "sha4".to_string()),
            ],
        ),
    );
}

/// A merge head that no branch names reads as its short sha, a detached HEAD
/// reads as `HEAD`, and more than one file left reads in the plural.
#[test]
fn the_merge_banner_names_unnamed_sides_and_counts_in_the_plural() {
    let merge = MergeState {
        head_sha: "f00dfeed1234".to_string(),
        head_name: None,
        message: String::new(),
        conflicted: 2,
    };
    let banner = merge_banner(&merge, None, 2);
    assert_eq!(
        (banner.head, banner.detail),
        (
            "f00dfee \u{2192} HEAD".to_string(),
            "2 files left to resolve".to_string()
        ),
    );
}

/// A read with fewer rows pulls the selection back onto its last row.
#[test]
fn a_read_with_fewer_rows_holds_the_selection_on_the_last() {
    let model = |rows: &[&str]| GitStatusModel {
        root: PathBuf::from(ROOT),
        branch: None,
        divergence: Divergence::Unknown,
        upstream: String::new(),
        op: None,
        sections: vec![section(
            SectionKind::Staged,
            rows.iter()
                .map(|rel| row(rel, RowKind::Modified, Note::None))
                .collect(),
        )],
    };
    let mut panel = GitStatusPanel::default();
    panel.land(model(&["a.rs", "b.rs", "c.rs"]));
    panel.step(2);
    panel.land(model(&["a.rs"]));

    assert_eq!(panel.selected, 0);
}

/// A chunk that only re-indents settles on its own, so it counts among the
/// file's chunks and not among the open ones.
#[test]
fn an_indent_only_chunk_counts_as_settled() {
    let model = read(
        |repo| {
            repo.conflicted_file(
                "indent.txt",
                Some("a\nb\nc\n"),
                Some("    a\n    b\n    c\n"),
                Some("a\nB\nc\n"),
            );
        },
        None,
        &HashMap::new(),
    );

    assert_eq!(
        model.sections,
        [section(
            SectionKind::Conflicted,
            vec![row(
                "indent.txt",
                RowKind::Unmerged,
                Note::Open { open: 0, total: 1 }
            )],
        )],
    );
}

/// A panel whose staged section holds `staged` rows and whose unstaged
/// section holds `unstaged` rows, indexed in that order.
fn two_sections(staged: usize, unstaged: usize) -> GitStatusPanel {
    let rows = |prefix: &str, count: usize| {
        (0..count)
            .map(|i| row(&format!("{prefix}{i}.rs"), RowKind::Modified, Note::None))
            .collect()
    };
    let mut panel = GitStatusPanel::default();
    panel.land(GitStatusModel {
        root: PathBuf::from(ROOT),
        branch: None,
        divergence: Divergence::Unknown,
        upstream: String::new(),
        op: None,
        sections: vec![
            section(SectionKind::Staged, rows("s", staged)),
            section(SectionKind::Unstaged, rows("u", unstaged)),
        ],
    });
    panel
}

/// The body's lines, as each head's section and each row's section and
/// index.
fn body(panel: &GitStatusPanel) -> Vec<(SectionKind, Option<usize>)> {
    panel
        .body_lines()
        .map(|line| match line {
            BodyLine::Head(section) => (section.kind, None),
            BodyLine::Row(section, index, _) => (section.kind, Some(index)),
        })
        .collect()
}

/// A body scrolled into a section keeps that section's head over its first
/// shown row, and the sections above it show nothing.
#[test]
fn a_scrolled_body_keeps_the_head_of_its_first_row() {
    let mut panel = two_sections(3, 2);
    panel.top = 2;
    let in_first = body(&panel);
    panel.top = 4;

    assert_eq!(
        (in_first, body(&panel)),
        (
            vec![
                (SectionKind::Staged, None),
                (SectionKind::Staged, Some(2)),
                (SectionKind::Unstaged, None),
                (SectionKind::Unstaged, Some(3)),
                (SectionKind::Unstaged, Some(4)),
            ],
            vec![
                (SectionKind::Unstaged, None),
                (SectionKind::Unstaged, Some(4))
            ],
        ),
    );
}

/// Walking down past the body's last line moves the top just far enough to
/// show the row, and walking back up puts the top on the row.
#[test]
fn the_top_follows_the_selection_down_and_back_up() {
    let mut panel = two_sections(5, 5);
    let tops: Vec<usize> = [3, 7, 9, 6, 0]
        .into_iter()
        .map(|selected| {
            panel.selected = selected;
            panel.scroll_into_view(6);
            panel.top
        })
        .collect();

    assert_eq!(tops, [0, 4, 5, 5, 0]);
}

/// A stage set where both sides edit the middle line, which leaves one chunk
/// open.
fn conflict(path: &str) -> ConflictedFile {
    ConflictedFile {
        path: PathBuf::from(path),
        ancestor: Some("a\nb\nc\n".to_string()),
        ours: Some("a\nB\nc\n".to_string()),
        theirs: Some("a\nX\nc\n".to_string()),
    }
}

/// A rebase paused on a conflict lists a file that waits for a side as a
/// conflicted row with its open chunks, and a resolved file as a staged row
/// under the side it took.
#[test]
fn a_rebase_pause_lists_its_conflicts() {
    let progress = RebaseProgress {
        done: 1,
        total: 2,
        onto: "c1".to_string(),
        head: "c1".to_string(),
        message: None,
        next: Vec::new(),
        conflicts: vec![
            (conflict("render/chrome.rs"), None),
            (conflict("theme.rs"), Some(Note::TookOurs)),
        ],
    };
    let model = read(|_| {}, Some(&progress), &HashMap::new());

    assert_eq!(
        model.sections,
        [
            section(
                SectionKind::Conflicted,
                vec![row(
                    "render/chrome.rs",
                    RowKind::Unmerged,
                    Note::Open { open: 1, total: 1 }
                )],
            ),
            section(
                SectionKind::Staged,
                vec![row("theme.rs", RowKind::Modified, Note::TookOurs)],
            ),
        ],
    );
}

/// The progress copies a conflict pause's files, each with the side its
/// resolution takes, and a skipped file reads as still waiting.
#[test]
fn rebase_progress_copies_the_pause_conflicts() {
    let mut active = ActiveRebase::new(
        RebaseState::new(
            PathBuf::from(ROOT),
            "base".to_string(),
            vec![entry(RebaseTodoOp::Pick, "sha0")],
        ),
        None,
    );
    active.pause = Some(RebasePause::Conflict {
        source_sha: "sha0".to_string(),
        files: vec![
            conflict("a.rs"),
            conflict("b.rs"),
            conflict("c.rs"),
            conflict("d.rs"),
        ],
        selected: 0,
        resolutions: HashMap::from([
            (PathBuf::from("a.rs"), ConflictResolution::TakeTheirs),
            (PathBuf::from("b.rs"), ConflictResolution::TakeOurs),
            (PathBuf::from("c.rs"), ConflictResolution::SkipEntry),
        ]),
        merge_rows: vec![None, None, None, None],
    });

    let copied: Vec<(PathBuf, Option<Note>)> = RebaseProgress::of(&active)
        .conflicts
        .into_iter()
        .map(|(file, note)| (file.path, note))
        .collect();
    assert_eq!(
        copied,
        [
            (PathBuf::from("a.rs"), Some(Note::TookTheirs)),
            (PathBuf::from("b.rs"), Some(Note::TookOurs)),
            (PathBuf::from("c.rs"), None),
            (PathBuf::from("d.rs"), None),
        ],
    );
}

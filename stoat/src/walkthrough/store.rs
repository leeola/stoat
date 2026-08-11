//! Where a workspace keeps its walkthroughs, and how they reach disk.
//!
//! Walkthroughs belong to a repository rather than to a machine, so they live
//! under the workspace root at `.stoat/walkthroughs/<slug>.json`, beside the
//! other workspace state stoat keeps. Cloning the repository brings the tours
//! along.
//!
//! The root is found by walking up to the enclosing git repository, which is
//! also what makes a stop's path meaningful: every [`Location`] is stored
//! relative to that root, so the same walkthrough resolves in any checkout.
//!
//! [`super`] holds the format and its pure operations. This module is the only
//! part of the feature that touches the filesystem, and it reaches it through
//! [`FsHost`] like the rest of stoat, so a caller substitutes a fake and the
//! store never opens a real file.

use crate::{host::FsHost, walkthrough::Walkthrough};
use git2::Repository;
use snafu::{Location as ErrorLocation, OptionExt, ResultExt, Snafu};
use std::{
    io,
    path::{Component, Path, PathBuf},
};

/// Characters a slug holds after its first, which is narrower still.
///
/// A slug becomes a filename, so the rule doubles as the guard against a name
/// that escapes the walkthroughs directory. Neither `/` nor `.` passes, so
/// `..` and an absolute path are both unrepresentable.
const SLUG_MAX: usize = 64;

/// One walkthrough as [`list`] reports it, without loading its stops.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Summary {
    pub slug: String,
    pub title: String,
    pub stops: usize,
}

/// Failure finding a workspace or moving a walkthrough to or from disk.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum StoreError {
    #[snafu(display("no git workspace at or above {}", start.display()))]
    NoWorkspace {
        start: PathBuf,
        #[snafu(implicit)]
        location: ErrorLocation,
    },

    #[snafu(display(
        "'{slug}' is not a slug: lowercase letters, digits, and dashes only, \
         starting with a letter or digit, at most {SLUG_MAX} characters"
    ))]
    InvalidSlug {
        slug: String,
        #[snafu(implicit)]
        location: ErrorLocation,
    },

    #[snafu(display("no walkthrough '{slug}'"))]
    UnknownWalkthrough {
        slug: String,
        #[snafu(implicit)]
        location: ErrorLocation,
    },

    #[snafu(display("{}: {source}", path.display()))]
    Io {
        path: PathBuf,
        source: io::Error,
        #[snafu(implicit)]
        location: ErrorLocation,
    },

    #[snafu(display("{}: {source}", path.display()))]
    Json {
        path: PathBuf,
        source: serde_json::Error,
        #[snafu(implicit)]
        location: ErrorLocation,
    },
}

/// The workspace root at or above `start`, being the enclosing repository's
/// working tree.
///
/// Errors when nothing above `start` is a repository, or when the repository
/// found is bare and so has no working tree to store anything in. A caller that
/// knows its root passes it directly rather than calling this.
pub fn workspace_root(start: &Path) -> Result<PathBuf, StoreError> {
    let repo = Repository::discover(start)
        .ok()
        .context(NoWorkspaceSnafu { start })?;
    let workdir = repo.workdir().context(NoWorkspaceSnafu { start })?;
    Ok(workdir.to_path_buf())
}

/// Where `root`'s walkthroughs live, whether or not the directory exists yet.
///
/// [`save`] creates it. Every other operation treats an absent directory as a
/// workspace with no walkthroughs rather than an error.
pub fn walkthroughs_dir(root: &Path) -> PathBuf {
    root.join(".stoat").join("walkthroughs")
}

/// Read the walkthrough `slug` from `root`.
pub fn load(fs: &dyn FsHost, root: &Path, slug: &str) -> Result<Walkthrough, StoreError> {
    let path = walkthrough_path(root, slug)?;

    let json = match read_text(fs, &path) {
        Ok(json) => json,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return UnknownWalkthroughSnafu { slug }.fail();
        },
        Err(source) => return Err(source).context(IoSnafu { path }),
    };

    serde_json::from_str(&json).context(JsonSnafu { path })
}

/// Write `walkthrough` to `root` under its own slug, refreshing its recorded
/// commit.
///
/// The write is atomic, so an interrupted save leaves the previous walkthrough
/// intact rather than a truncated one.
///
/// `git_head` is taken from the workspace as it is now, not from what the
/// caller holds. A stop captured against one commit and saved after another
/// then records where the file actually was.
pub fn save(fs: &dyn FsHost, root: &Path, walkthrough: &Walkthrough) -> Result<(), StoreError> {
    let path = walkthrough_path(root, &walkthrough.slug)?;
    let dir = walkthroughs_dir(root);
    fs.create_dir_all(&dir).context(IoSnafu { path: &dir })?;

    let json = {
        let mut stored = walkthrough.clone();
        stored.git_head = head_commit(root);
        // A trailing newline, so the file reads as text to everything that
        // expects one, git diffs included.
        let mut json = serde_json::to_string_pretty(&stored).context(JsonSnafu { path: &path })?;
        json.push('\n');
        json
    };

    fs.write_atomic(&path, json.as_bytes())
        .context(IoSnafu { path })
}

/// Every walkthrough in `root`, in slug order.
///
/// A workspace with no walkthroughs directory has no walkthroughs, so that is
/// an empty list rather than an error.
///
/// A file that does not parse fails the whole call. The result has no room to
/// report one bad entry, and naming the broken file is more use than quietly
/// dropping a walkthrough the user knows they wrote.
pub fn list(fs: &dyn FsHost, root: &Path) -> Result<Vec<Summary>, StoreError> {
    let dir = walkthroughs_dir(root);
    let entries = match fs.list_dir(&dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(source).context(IoSnafu { path: dir }),
    };

    let mut summaries = Vec::new();
    for entry in entries {
        if entry.is_dir || !entry.name.ends_with(".json") {
            continue;
        }

        let path = dir.join(entry.name.as_str());
        let json = read_text(fs, &path).context(IoSnafu { path: &path })?;
        let walkthrough: Walkthrough =
            serde_json::from_str(&json).context(JsonSnafu { path: &path })?;
        summaries.push(Summary {
            slug: walkthrough.slug,
            title: walkthrough.title,
            stops: walkthrough.stops.len(),
        });
    }

    summaries.sort_by(|left, right| left.slug.cmp(&right.slug));
    Ok(summaries)
}

/// Remove the walkthrough `slug` from `root`.
pub fn delete(fs: &dyn FsHost, root: &Path, slug: &str) -> Result<(), StoreError> {
    let path = walkthrough_path(root, slug)?;
    match fs.remove_file(&path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            UnknownWalkthroughSnafu { slug }.fail()
        },
        Err(source) => Err(source).context(IoSnafu { path }),
    }
}

/// A reader over `root`'s files, shaped for [`super::validate`].
///
/// Resolves each workspace-relative path against `root` and returns its
/// content, or `None` when the file is gone. `None` is what turns a moved or
/// deleted file into a finding rather than an error, so an unreadable file and
/// a missing one are deliberately the same answer.
///
/// A path that leaves the workspace reads as `None` too. The check is on the
/// path's own components rather than on a canonicalized result, because
/// canonicalizing fails for a file that no longer exists, which is exactly the
/// case validation exists to report.
pub fn workspace_reader<'a>(fs: &'a dyn FsHost, root: &Path) -> impl Fn(&Path) -> Option<String> {
    let root = root.to_path_buf();
    move |relative: &Path| {
        if !stays_within(relative) {
            return None;
        }
        read_text(fs, &root.join(relative)).ok()
    }
}

/// The file at `path` as text.
///
/// Content that is not UTF-8 comes back as an `InvalidData` io error rather
/// than lossily, since a walkthrough file that decodes with replacement
/// characters is not the file that was written.
fn read_text(fs: &dyn FsHost, path: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    fs.read(path, &mut bytes)?;
    String::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.utf8_error()))
}

/// Whether `path` names something inside the workspace it is resolved against.
///
/// A relative path made only of normal components has no way out, so those are
/// the only ones accepted. Everything else, including a bare `..` and any
/// absolute or root-prefixed form, is refused.
fn stays_within(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// The path the walkthrough `slug` occupies under `root`.
///
/// Validates the slug first, so a name that escapes the directory never reaches
/// the filesystem.
fn walkthrough_path(root: &Path, slug: &str) -> Result<PathBuf, StoreError> {
    if !is_slug(slug) {
        return InvalidSlugSnafu { slug }.fail();
    }
    Ok(walkthroughs_dir(root).join(format!("{slug}.json")))
}

/// Whether `slug` is a legal walkthrough name.
///
/// Lowercase letters, digits, and dashes, starting with a letter or digit, and
/// at most [`SLUG_MAX`] characters. The leading-character rule keeps a name
/// from reading as a flag when it reaches a command line, and excluding `.` and
/// `/` is what makes the slug safe to join onto a directory.
fn is_slug(slug: &str) -> bool {
    let leads = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();

    !slug.is_empty()
        && slug.len() <= SLUG_MAX
        && slug.bytes().next().is_some_and(leads)
        && slug.bytes().all(|byte| leads(byte) || byte == b'-')
}

/// The commit `root`'s repository is on, as hex.
///
/// `None` outside a repository and before the first commit, since neither has a
/// commit to name. Both are ordinary states for a workspace someone is only
/// starting, so neither is an error.
fn head_commit(root: &Path) -> Option<String> {
    let repo = Repository::discover(root).ok()?;
    let head = repo.head().ok()?;
    Some(head.peel_to_commit().ok()?.id().to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        delete, head_commit, list, load, read_text, save, walkthroughs_dir, workspace_reader,
        workspace_root, Summary,
    };
    use crate::{
        host::{FsHost, LocalFs},
        walkthrough::{Location, Point, Range, Walkthrough},
    };
    use git2::{Repository, Signature};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn tour(slug: &str, title: &str) -> Walkthrough {
        let mut walkthrough = Walkthrough::new(slug.to_owned(), title.to_owned(), None);
        walkthrough
            .add_stop(
                None,
                "narration".to_owned(),
                Location {
                    path: PathBuf::from("a.rs"),
                    range: Range {
                        start: Point { line: 1, col: 1 },
                        end: Point { line: 1, col: 1 },
                    },
                    snippet: "x".to_owned(),
                },
                None,
            )
            .expect("append needs no anchor");
        walkthrough
    }

    /// A repository with one commit, so `head_commit` has something to find.
    fn repo_with_commit() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path()).expect("init");

        LocalFs
            .write(&dir.path().join("a.rs"), b"x\n")
            .expect("write");
        let mut index = repo.index().expect("index");
        index.add_path(Path::new("a.rs")).expect("add");
        index.write().expect("write index");
        let tree = repo
            .find_tree(index.write_tree().expect("write tree"))
            .expect("find tree");

        let sig = Signature::now("test", "t@t").expect("signature");
        repo.commit(Some("HEAD"), &sig, &sig, "c", &tree, &[])
            .expect("commit");

        dir
    }

    #[test]
    fn a_saved_walkthrough_loads_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let saved = tour("intro", "Intro");

        save(&LocalFs, dir.path(), &saved).expect("save");
        assert_eq!(load(&LocalFs, dir.path(), "intro").expect("load"), saved);
    }

    #[test]
    fn a_save_creates_the_walkthroughs_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!walkthroughs_dir(dir.path()).exists(), "nothing there yet");

        save(&LocalFs, dir.path(), &tour("intro", "Intro")).expect("save");
        assert!(walkthroughs_dir(dir.path()).join("intro.json").exists());
    }

    /// The file is read by people and by git, both of which expect text.
    #[test]
    fn a_saved_file_is_pretty_json_ending_in_a_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        save(&LocalFs, dir.path(), &tour("intro", "Intro")).expect("save");

        let json =
            read_text(&LocalFs, &walkthroughs_dir(dir.path()).join("intro.json")).expect("read");
        assert!(json.ends_with("}\n"), "got {:?}", &json[json.len() - 8..]);
        assert!(json.contains("\n  \"slug\""), "indented, got {json}");
    }

    #[test]
    fn a_second_save_replaces_the_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        save(&LocalFs, dir.path(), &tour("intro", "Intro")).expect("save");
        save(&LocalFs, dir.path(), &tour("intro", "Renamed")).expect("save");

        assert_eq!(
            load(&LocalFs, dir.path(), "intro").expect("load").title,
            "Renamed"
        );
        assert_eq!(
            list(&LocalFs, dir.path()).expect("list").len(),
            1,
            "the temp file the atomic write used is gone",
        );
    }

    #[test]
    fn list_reports_every_walkthrough_in_slug_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        save(&LocalFs, dir.path(), &tour("zebra", "Z")).expect("save");
        save(&LocalFs, dir.path(), &tour("alpha", "A")).expect("save");

        assert_eq!(
            list(&LocalFs, dir.path()).expect("list"),
            vec![
                Summary {
                    slug: "alpha".to_owned(),
                    title: "A".to_owned(),
                    stops: 1,
                },
                Summary {
                    slug: "zebra".to_owned(),
                    title: "Z".to_owned(),
                    stops: 1,
                },
            ]
        );
    }

    /// A workspace nobody has written a walkthrough in is empty, not broken.
    #[test]
    fn list_finds_nothing_before_the_first_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(list(&LocalFs, dir.path()).expect("list"), Vec::new());
    }

    #[test]
    fn delete_removes_the_walkthrough() {
        let dir = tempfile::tempdir().expect("tempdir");
        save(&LocalFs, dir.path(), &tour("intro", "Intro")).expect("save");

        delete(&LocalFs, dir.path(), "intro").expect("delete");
        assert_eq!(list(&LocalFs, dir.path()).expect("list"), Vec::new());
        assert!(
            delete(&LocalFs, dir.path(), "intro").is_err(),
            "already gone"
        );
    }

    #[test]
    fn loading_a_slug_nobody_saved_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load(&LocalFs, dir.path(), "missing").is_err());
    }

    /// The slug becomes a filename, so the rule is also what keeps a name from
    /// reaching outside the walkthroughs directory.
    #[test]
    fn a_slug_that_is_not_a_slug_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        for slug in [
            "",
            "-lead",
            "Upper",
            "has space",
            "dot.ted",
            "a/b",
            "..",
            &"x".repeat(65),
        ] {
            assert!(load(&LocalFs, dir.path(), slug).is_err(), "loaded {slug:?}");
            assert!(
                delete(&LocalFs, dir.path(), slug).is_err(),
                "deleted {slug:?}"
            );
            assert!(
                save(&LocalFs, dir.path(), &tour(slug, "T")).is_err(),
                "saved {slug:?}",
            );
        }

        for slug in ["a", "0", "intro", "two-part-name", &"x".repeat(64)] {
            assert!(
                save(&LocalFs, dir.path(), &tour(slug, "T")).is_ok(),
                "refused {slug:?}"
            );
        }
    }

    #[test]
    fn save_records_the_commit_the_workspace_is_on() {
        let dir = repo_with_commit();
        let head = head_commit(dir.path()).expect("the repo has a commit");

        save(&LocalFs, dir.path(), &tour("intro", "Intro")).expect("save");
        assert_eq!(
            load(&LocalFs, dir.path(), "intro").expect("load").git_head,
            Some(head),
            "the saved head is the workspace's, not the caller's",
        );
    }

    #[test]
    fn a_workspace_with_no_commit_records_none() {
        let outside = tempfile::tempdir().expect("tempdir");
        save(&LocalFs, outside.path(), &tour("intro", "Intro")).expect("save");
        assert_eq!(
            load(&LocalFs, outside.path(), "intro")
                .expect("load")
                .git_head,
            None
        );

        let fresh = tempfile::tempdir().expect("tempdir");
        Repository::init(fresh.path()).expect("init");
        save(&LocalFs, fresh.path(), &tour("intro", "Intro")).expect("save");
        assert_eq!(
            load(&LocalFs, fresh.path(), "intro")
                .expect("load")
                .git_head,
            None,
            "a repository before its first commit names no commit",
        );
    }

    #[test]
    fn workspace_root_finds_the_enclosing_repository() {
        let dir = repo_with_commit();
        let nested = dir.path().join("src").join("deep");
        LocalFs.create_dir_all(&nested).expect("mkdir");

        let found = workspace_root(&nested).expect("inside a repository");
        assert_eq!(
            found.canonicalize().expect("canonicalize"),
            dir.path().canonicalize().expect("canonicalize"),
        );
    }

    #[test]
    fn workspace_root_needs_a_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            workspace_root(dir.path()).is_err(),
            "a bare directory is not a workspace"
        );
    }

    #[test]
    fn the_reader_reads_inside_the_workspace_and_nothing_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        LocalFs
            .write(&dir.path().join("a.rs"), b"content\n")
            .expect("write");
        let read = workspace_reader(&LocalFs, dir.path());

        assert_eq!(read(Path::new("a.rs")).as_deref(), Some("content\n"));
        assert_eq!(
            read(Path::new("gone.rs")),
            None,
            "a missing file reads None"
        );
        assert_eq!(
            read(Path::new("../a.rs")),
            None,
            "and so does one that climbs out",
        );
        assert_eq!(
            read(Path::new("/etc/hostname")),
            None,
            "and an absolute path, whatever it points at",
        );
    }
}

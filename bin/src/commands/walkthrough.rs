//! The `stoat walkthrough` CRUD surface over a workspace's walkthroughs.
//!
//! A walkthrough is authored out of band rather than from inside the editor,
//! so this is how one is created, inspected, and removed. The file it manages
//! is plain JSON under `.stoat/walkthroughs/`, which is what lets an agent
//! write the stops and annotations directly and use these commands only for
//! the lifecycle around them.
//!
//! Each subcommand returns its output text and leaves the printing to [`run`],
//! so the output is asserted directly rather than through a captured stdout.

use clap::Subcommand;
use snafu::{whatever, ResultExt, Whatever};
use std::path::{Path, PathBuf};
use stoat::{
    host::{FsHost, LocalFs},
    walkthrough::{
        store::{self, StoreError},
        Walkthrough,
    },
};

/// Subcommands managing the walkthroughs stored in a workspace.
#[derive(Subcommand, Debug)]
pub enum WalkthroughCommand {
    /// Create an empty walkthrough and print the file it wrote.
    New {
        /// Names the file and addresses the walkthrough. Lowercase letters,
        /// digits, and dashes.
        slug: String,

        /// Human-readable title. Defaults to the slug.
        #[arg(long)]
        title: Option<String>,

        /// Workspace root. Defaults to the repository enclosing the current
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },

    /// Print every walkthrough as `slug`, stop count, and title, tab separated.
    List {
        /// Workspace root. Defaults to the repository enclosing the current
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },

    /// Print a walkthrough's stored JSON.
    Show {
        slug: String,

        /// Workspace root. Defaults to the repository enclosing the current
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },

    /// Remove a walkthrough and print the file it deleted.
    Delete {
        slug: String,

        /// Workspace root. Defaults to the repository enclosing the current
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

pub fn run(sub: WalkthroughCommand) -> Result<(), Whatever> {
    let text = match sub {
        WalkthroughCommand::New {
            slug,
            title,
            workspace,
        } => {
            let root = resolve_root(workspace.as_deref())?;
            new(&LocalFs, &root, &slug, title.as_deref())
        },
        WalkthroughCommand::List { workspace } => {
            let root = resolve_root(workspace.as_deref())?;
            list(&LocalFs, &root)
        },
        WalkthroughCommand::Show { slug, workspace } => {
            let root = resolve_root(workspace.as_deref())?;
            show(&LocalFs, &root, &slug)
        },
        WalkthroughCommand::Delete { slug, workspace } => {
            let root = resolve_root(workspace.as_deref())?;
            delete(&LocalFs, &root, &slug)
        },
    }?;

    println!("{text}");
    Ok(())
}

/// The workspace to operate on, being `workspace` when given and otherwise the
/// repository enclosing the current directory.
///
/// An explicit root is taken as-is rather than searched upward from, so a
/// caller that already knows where the workspace is never has the answer
/// second-guessed.
fn resolve_root(workspace: Option<&Path>) -> Result<PathBuf, Whatever> {
    let Some(explicit) = workspace else {
        let cwd = std::env::current_dir().whatever_context("read the current directory")?;
        return store::workspace_root(&cwd)
            .whatever_context(format!("find the workspace enclosing {}", cwd.display()));
    };
    Ok(explicit.to_path_buf())
}

/// Create the walkthrough `slug`, returning the file it wrote.
///
/// The title defaults to the slug, so a walkthrough always has one to show in a
/// listing even when the author names it later.
///
/// Refuses a slug that already exists, since the alternative overwrites
/// authored stops with an empty file. A slug whose file exists but does not
/// parse reports the parse failure instead, which is the more useful answer for
/// a user whose file is broken.
fn new(fs: &dyn FsHost, root: &Path, slug: &str, title: Option<&str>) -> Result<String, Whatever> {
    match store::load(fs, root, slug) {
        Ok(_) => whatever!("walkthrough '{slug}' already exists"),
        Err(StoreError::UnknownWalkthrough { .. }) => {},
        Err(error) => {
            return Err(error).whatever_context(format!("read the existing walkthrough '{slug}'"))
        },
    }

    let title = title.unwrap_or(slug).to_owned();
    let walkthrough = Walkthrough::new(slug.to_owned(), title, None);
    store::save(fs, root, &walkthrough).whatever_context(format!("write walkthrough '{slug}'"))?;

    Ok(relative_path(root, slug))
}

/// Every walkthrough in `root` as one tab-separated line each, in slug order.
///
/// Tabs rather than aligned columns, so the output stays a record per line for
/// whatever reads it next. An empty workspace produces no lines.
fn list(fs: &dyn FsHost, root: &Path) -> Result<String, Whatever> {
    let summaries = store::list(fs, root)
        .whatever_context(format!("list the walkthroughs in {}", root.display()))?;
    Ok(summaries
        .iter()
        .map(|summary| format!("{}\t{}\t{}", summary.slug, summary.stops, summary.title))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// The walkthrough `slug` as the JSON it is stored in.
///
/// Re-serialized rather than echoed byte for byte, which reads the same because
/// the store wrote it with the same formatting.
fn show(fs: &dyn FsHost, root: &Path, slug: &str) -> Result<String, Whatever> {
    let walkthrough =
        store::load(fs, root, slug).whatever_context(format!("read walkthrough '{slug}'"))?;
    serde_json::to_string_pretty(&walkthrough)
        .whatever_context(format!("render walkthrough '{slug}'"))
}

/// Remove the walkthrough `slug`, returning the file it deleted.
fn delete(fs: &dyn FsHost, root: &Path, slug: &str) -> Result<String, Whatever> {
    store::delete(fs, root, slug).whatever_context(format!("delete walkthrough '{slug}'"))?;
    Ok(relative_path(root, slug))
}

/// Where `slug` lives, written relative to the workspace.
///
/// Relative because that is how the user thinks of a file in their repository,
/// and because an absolute path in a command's output is noise in a transcript
/// someone else reads later.
fn relative_path(root: &Path, slug: &str) -> String {
    store::walkthroughs_dir(root)
        .strip_prefix(root)
        .unwrap_or(&store::walkthroughs_dir(root))
        .join(format!("{slug}.json"))
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{delete, list, new, show};
    use git2::Repository;
    use std::path::Path;
    use stoat::host::LocalFs;
    use tempfile::TempDir;

    /// A workspace to operate in. A repository, since that is what
    /// `workspace_root` discovers, though these tests pass the root directly.
    fn workspace() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        Repository::init(dir.path()).expect("init");
        dir
    }

    #[test]
    fn new_writes_the_file_and_prints_where() {
        let dir = workspace();

        let printed = new(&LocalFs, dir.path(), "intro", Some("Getting started")).expect("new");
        assert_eq!(printed, ".stoat/walkthroughs/intro.json");
        assert!(dir.path().join(&printed).exists(), "the file is there");
    }

    /// A walkthrough carries authored stops, so a create over one throws away
    /// work that took a person to write.
    #[test]
    fn new_refuses_a_slug_that_is_taken() {
        let dir = workspace();
        new(&LocalFs, dir.path(), "intro", None).expect("new");

        assert!(new(&LocalFs, dir.path(), "intro", None).is_err());
    }

    #[test]
    fn a_new_walkthrough_titles_itself_after_its_slug() {
        let dir = workspace();
        new(&LocalFs, dir.path(), "intro", None).expect("new");

        assert_eq!(
            list(&LocalFs, dir.path()).expect("list"),
            "intro\t0\tintro",
            "so a listing has something to show before the author names it",
        );
    }

    #[test]
    fn list_prints_one_tab_separated_line_per_walkthrough_in_slug_order() {
        let dir = workspace();
        new(&LocalFs, dir.path(), "zebra", Some("Z")).expect("new");
        new(&LocalFs, dir.path(), "alpha", Some("A")).expect("new");

        assert_eq!(
            list(&LocalFs, dir.path()).expect("list"),
            "alpha\t0\tA\nzebra\t0\tZ"
        );
    }

    #[test]
    fn list_is_empty_before_the_first_walkthrough() {
        let dir = workspace();
        assert_eq!(list(&LocalFs, dir.path()).expect("list"), "");
    }

    #[test]
    fn show_prints_the_stored_json() {
        let dir = workspace();
        new(&LocalFs, dir.path(), "intro", Some("Getting started")).expect("new");

        let printed = show(&LocalFs, dir.path(), "intro").expect("show");
        let stored = std::fs::read_to_string(
            dir.path()
                .join(".stoat")
                .join("walkthroughs")
                .join("intro.json"),
        )
        .expect("read");
        assert_eq!(
            format!("{printed}\n"),
            stored,
            "the printed JSON is the file, bar its trailing newline",
        );
    }

    #[test]
    fn show_needs_the_walkthrough_to_exist() {
        let dir = workspace();
        assert!(show(&LocalFs, dir.path(), "missing").is_err());
    }

    #[test]
    fn delete_removes_the_file_and_prints_where() {
        let dir = workspace();
        new(&LocalFs, dir.path(), "intro", None).expect("new");

        let printed = delete(&LocalFs, dir.path(), "intro").expect("delete");
        assert_eq!(printed, ".stoat/walkthroughs/intro.json");
        assert!(!dir.path().join(&printed).exists(), "the file is gone");
        assert_eq!(list(&LocalFs, dir.path()).expect("list"), "");
    }

    #[test]
    fn delete_needs_the_walkthrough_to_exist() {
        let dir = workspace();
        assert!(delete(&LocalFs, dir.path(), "missing").is_err());
    }

    /// The slug rule belongs to the store, but a bad one has to surface as a
    /// command failure rather than a file named something surprising.
    #[test]
    fn a_bad_slug_fails_the_command() {
        let dir = workspace();
        assert!(new(&LocalFs, dir.path(), "../escape", None).is_err());
        assert!(new(&LocalFs, dir.path(), "Upper", None).is_err());
        assert!(show(&LocalFs, Path::new("/nowhere"), "..").is_err());
    }
}

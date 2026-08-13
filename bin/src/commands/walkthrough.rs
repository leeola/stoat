//! The `stoat walkthrough` CRUD surface over a workspace's walkthroughs.
//!
//! A walkthrough is authored out of band rather than from inside the editor,
//! so this is how one is created, filled in, and removed. The file it manages
//! is plain JSON under `.stoat/walkthroughs/`, and these commands own the
//! parts an author has no business hand-writing. Those are the id assignment,
//! the ordering, and above all the captured snippets, which have to be read out
//! of the real file to be worth comparing later.
//!
//! Each subcommand returns its output text and leaves the printing to [`run`],
//! so the output is asserted directly rather than through a captured stdout.

use clap::{Args, Subcommand};
use snafu::{whatever, OptionExt, ResultExt, Whatever};
use std::path::{Path, PathBuf};
use stoat::{
    host::{FsHost, LocalFs},
    walkthrough::{
        self,
        store::{self, StoreError},
        Annotation, AnnotationEdit, Finding, FindingKind, Location, MoveTarget, Point, Range,
        StopEdit, Walkthrough,
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

        #[command(flatten)]
        workspace: WorkspaceArgs,
    },

    /// Print every walkthrough as `slug`, stop count, and title, tab separated.
    List {
        #[command(flatten)]
        workspace: WorkspaceArgs,
    },

    /// Print a walkthrough's stored JSON.
    Show {
        slug: String,

        #[command(flatten)]
        workspace: WorkspaceArgs,
    },

    /// Remove a walkthrough and print the file it deleted.
    Delete {
        slug: String,

        #[command(flatten)]
        workspace: WorkspaceArgs,
    },

    /// Add a stop focused on a range of a file, and print its new id.
    AddStop {
        slug: String,

        /// File the stop is about. Must be inside the workspace.
        #[arg(long)]
        file: PathBuf,

        /// Range within the file, as `L`, `L-L`, or `L:C-L:C`.
        #[arg(long)]
        range: String,

        /// Short heading for the stop.
        #[arg(long)]
        title: Option<String>,

        #[command(flatten)]
        narration: NarrationArgs,

        /// Insert before this stop rather than appending.
        #[arg(long)]
        before: Option<String>,

        #[command(flatten)]
        workspace: WorkspaceArgs,
    },

    /// Change a stop's focus, title, or narration, and print its id.
    EditStop {
        slug: String,
        stop: String,

        /// New file for the focus. Re-captures the snippet.
        #[arg(long)]
        file: Option<PathBuf>,

        /// New range for the focus. Re-captures the snippet.
        #[arg(long)]
        range: Option<String>,

        #[arg(long)]
        title: Option<String>,

        /// Drop the stop's title.
        #[arg(long, conflicts_with = "title")]
        no_title: bool,

        #[command(flatten)]
        narration: NarrationArgs,

        #[command(flatten)]
        workspace: WorkspaceArgs,
    },

    /// Remove a stop and its annotations, and print its id.
    RemoveStop {
        slug: String,
        stop: String,

        #[command(flatten)]
        workspace: WorkspaceArgs,
    },

    /// Move a stop within the walkthrough, and print its id.
    MoveStop {
        slug: String,
        stop: String,

        #[command(flatten)]
        to: MoveArgs,

        #[command(flatten)]
        workspace: WorkspaceArgs,
    },

    /// Add a labeled annotation to a stop, and print its new id.
    AddAnnotation {
        slug: String,
        stop: String,

        /// File the annotation is about. Must be inside the workspace. Omit for
        /// the stop's own focus file.
        #[arg(long)]
        file: Option<PathBuf>,

        /// Range within that file, as `L`, `L-L`, or `L:C-L:C`.
        #[arg(long)]
        range: String,

        #[arg(long)]
        label: String,

        #[command(flatten)]
        workspace: WorkspaceArgs,
    },

    /// Change an annotation's file, range, or label, and print its id.
    EditAnnotation {
        slug: String,
        stop: String,
        annotation: String,

        /// New file for the annotation. Re-captures the snippet.
        #[arg(long)]
        file: Option<PathBuf>,

        /// Return the annotation to its stop's focus file. Re-captures the
        /// snippet.
        #[arg(long, conflicts_with = "file")]
        no_file: bool,

        /// New range. Re-captures the snippet.
        #[arg(long)]
        range: Option<String>,

        #[arg(long)]
        label: Option<String>,

        #[command(flatten)]
        workspace: WorkspaceArgs,
    },

    /// Remove an annotation and print its id.
    RemoveAnnotation {
        slug: String,
        stop: String,
        annotation: String,

        #[command(flatten)]
        workspace: WorkspaceArgs,
    },

    /// Report every range whose file or captured bytes have moved on. Prints
    /// nothing and exits 0 when every walkthrough still matches.
    Check {
        /// Check only this walkthrough. Omit to check them all.
        slug: Option<String>,

        #[command(flatten)]
        workspace: WorkspaceArgs,
    },
}

/// Which workspace a subcommand operates on.
///
/// Flattened into every subcommand rather than repeated, since all of them need
/// it and none of them treat it differently.
#[derive(Args, Debug)]
pub struct WorkspaceArgs {
    /// Workspace root. Defaults to the repository enclosing the current
    /// directory.
    #[arg(long)]
    workspace: Option<PathBuf>,
}

/// Where a stop's narration text comes from.
///
/// The two sources are exclusive, and both are optional so an edit that touches
/// neither leaves the narration alone. The file form exists because narration
/// is markdown that runs to paragraphs, which is painful to pass as one shell
/// argument.
#[derive(Args, Debug)]
#[group(multiple = false)]
pub struct NarrationArgs {
    /// Markdown narration, given inline.
    #[arg(long)]
    narration: Option<String>,

    /// Read the markdown narration from a file, or from stdin for `-`.
    #[arg(long)]
    narration_file: Option<PathBuf>,
}

/// Where [`WalkthroughCommand::MoveStop`] puts the stop.
///
/// Exactly one, enforced by clap rather than checked at run time, so naming two
/// destinations fails at the command line with a usage message.
#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
pub struct MoveArgs {
    /// Put the stop before this one.
    #[arg(long)]
    before: Option<String>,

    /// Put the stop after this one.
    #[arg(long)]
    after: Option<String>,

    /// Put the stop at the end.
    #[arg(long)]
    last: bool,
}

/// A range as the command line spells it, before a file says how long its lines
/// are.
///
/// The bare-line forms cover whole lines, and where a line ends depends on the
/// content. Keeping that unresolved is what lets [`parse_range`] stay pure, and
/// keeps a [`Range`] from ever holding a column that names no byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RangeSpec {
    /// `L` or `L-L`, covering those lines whole.
    Lines { start: u32, end: u32 },
    /// `L:C-L:C`, naming exact bytes.
    Points(Range),
}

pub fn run(sub: WalkthroughCommand) -> Result<(), Whatever> {
    let fs = &LocalFs;
    let text = match sub {
        WalkthroughCommand::Check { slug, workspace } => {
            return report(check(fs, &workspace.resolve()?, slug.as_deref())?);
        },

        WalkthroughCommand::New {
            slug,
            title,
            workspace,
        } => new(fs, &workspace.resolve()?, &slug, title.as_deref()),

        WalkthroughCommand::List { workspace } => list(fs, &workspace.resolve()?),

        WalkthroughCommand::Show { slug, workspace } => show(fs, &workspace.resolve()?, &slug),

        WalkthroughCommand::Delete { slug, workspace } => delete(fs, &workspace.resolve()?, &slug),

        WalkthroughCommand::AddStop {
            slug,
            file,
            range,
            title,
            narration,
            before,
            workspace,
        } => add_stop(
            fs,
            &workspace.resolve()?,
            &slug,
            &file,
            &range,
            title,
            narration.read()?.unwrap_or_default(),
            before.as_deref(),
        ),

        WalkthroughCommand::EditStop {
            slug,
            stop,
            file,
            range,
            title,
            no_title,
            narration,
            workspace,
        } => edit_stop(
            fs,
            &workspace.resolve()?,
            &slug,
            &stop,
            file.as_deref(),
            range.as_deref(),
            title_edit(title, no_title),
            narration.read()?,
        ),

        WalkthroughCommand::RemoveStop {
            slug,
            stop,
            workspace,
        } => remove_stop(fs, &workspace.resolve()?, &slug, &stop),

        WalkthroughCommand::MoveStop {
            slug,
            stop,
            to,
            workspace,
        } => move_stop(fs, &workspace.resolve()?, &slug, &stop, &to),

        WalkthroughCommand::AddAnnotation {
            slug,
            stop,
            file,
            range,
            label,
            workspace,
        } => add_annotation(
            fs,
            &workspace.resolve()?,
            &slug,
            &stop,
            file.as_deref(),
            &range,
            label,
        ),

        WalkthroughCommand::EditAnnotation {
            slug,
            stop,
            annotation,
            file,
            no_file,
            range,
            label,
            workspace,
        } => edit_annotation(
            fs,
            &workspace.resolve()?,
            &slug,
            &stop,
            &annotation,
            file.as_deref(),
            no_file,
            range.as_deref(),
            label,
        ),

        WalkthroughCommand::RemoveAnnotation {
            slug,
            stop,
            annotation,
            workspace,
        } => remove_annotation(fs, &workspace.resolve()?, &slug, &stop, &annotation),
    }?;

    println!("{text}");
    Ok(())
}

impl WorkspaceArgs {
    /// The workspace to operate on, being the given root or the repository
    /// enclosing the current directory.
    ///
    /// An explicit root is taken as-is rather than searched upward from, so a
    /// caller that already knows where the workspace is never has the answer
    /// second-guessed.
    fn resolve(&self) -> Result<PathBuf, Whatever> {
        let Some(explicit) = &self.workspace else {
            let cwd = std::env::current_dir().whatever_context("read the current directory")?;
            return store::workspace_root(&cwd)
                .whatever_context(format!("find the workspace enclosing {}", cwd.display()));
        };
        Ok(explicit.clone())
    }
}

impl NarrationArgs {
    /// The narration these flags name, or `None` when neither was given.
    ///
    /// `-` reads stdin, which is how a long markdown narration arrives without
    /// becoming one enormous shell argument.
    fn read(&self) -> Result<Option<String>, Whatever> {
        if let Some(inline) = &self.narration {
            return Ok(Some(inline.clone()));
        }
        let Some(path) = &self.narration_file else {
            return Ok(None);
        };

        let text = match path.as_os_str() == "-" {
            true => std::io::read_to_string(std::io::stdin())
                .whatever_context("read the narration from stdin")?,
            false => std::fs::read_to_string(path)
                .whatever_context(format!("read the narration from {}", path.display()))?,
        };
        Ok(Some(text))
    }
}

impl MoveArgs {
    /// The destination these flags name.
    fn target(&self) -> MoveTarget<'_> {
        match (&self.before, &self.after) {
            (Some(id), _) => MoveTarget::Before(id),
            (_, Some(id)) => MoveTarget::After(id),
            _ => MoveTarget::Last,
        }
    }
}

/// Parse a `--range` argument.
///
/// Accepts `L` for one whole line, `L-L` for a span of whole lines, and
/// `L:C-L:C` for exact byte positions. Lines and columns are 1-based and both
/// ends are inclusive, matching how an editor reports a selection.
///
/// A mixed form such as `3-4:2` is refused rather than guessed at. Half a range
/// in columns means the author meant columns for both ends, so accepting it
/// silently widens or narrows the span they asked for.
pub fn parse_range(text: &str) -> Result<RangeSpec, Whatever> {
    let Some((start, end)) = text.split_once('-') else {
        let line = parse_line(text)?;
        return Ok(RangeSpec::Lines {
            start: line,
            end: line,
        });
    };

    match (start.split_once(':'), end.split_once(':')) {
        (None, None) => Ok(RangeSpec::Lines {
            start: parse_line(start)?,
            end: parse_line(end)?,
        }),
        (Some((start_line, start_col)), Some((end_line, end_col))) => {
            Ok(RangeSpec::Points(Range {
                start: Point {
                    line: parse_line(start_line)?,
                    col: parse_line(start_col)?,
                },
                end: Point {
                    line: parse_line(end_line)?,
                    col: parse_line(end_col)?,
                },
            }))
        },
        _ => whatever!("'{text}' mixes a line with a line:column, which is not a range"),
    }
}

/// One 1-based number of a range.
fn parse_line(text: &str) -> Result<u32, Whatever> {
    let value: u32 = text
        .parse()
        .ok()
        .with_whatever_context(|| format!("'{text}' is not a number"))?;
    match value {
        0 => whatever!("lines and columns start at 1, so 0 names nothing"),
        value => Ok(value),
    }
}

/// Resolve `spec` against `content`, filling in where the lines it names end.
fn resolve_range(spec: RangeSpec, content: &str) -> Result<Range, Whatever> {
    let (start, end) = match spec {
        RangeSpec::Points(range) => return Ok(range),
        RangeSpec::Lines { start, end } => (start, end),
    };

    let bytes = content
        .lines()
        .nth(end as usize - 1)
        .with_whatever_context(|| format!("the file has no line {end}"))?
        .len();
    if bytes == 0 {
        whatever!("line {end} is empty, so no range covers it");
    }

    Ok(Range {
        start: Point {
            line: start,
            col: 1,
        },
        end: Point {
            line: end,
            col: bytes as u32,
        },
    })
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
            return Err(error).whatever_context(format!("read the existing walkthrough '{slug}'"));
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
    let walkthrough = load(fs, root, slug)?;
    serde_json::to_string_pretty(&walkthrough)
        .whatever_context(format!("render walkthrough '{slug}'"))
}

/// Remove the walkthrough `slug`, returning the file it deleted.
fn delete(fs: &dyn FsHost, root: &Path, slug: &str) -> Result<String, Whatever> {
    store::delete(fs, root, slug).whatever_context(format!("delete walkthrough '{slug}'"))?;
    Ok(relative_path(root, slug))
}

/// Add a stop focused on `range` of `file`, returning its new id.
#[allow(clippy::too_many_arguments)]
fn add_stop(
    fs: &dyn FsHost,
    root: &Path,
    slug: &str,
    file: &Path,
    range: &str,
    title: Option<String>,
    narration: String,
    before: Option<&str>,
) -> Result<String, Whatever> {
    let mut walkthrough = load(fs, root, slug)?;
    let focus = capture(fs, root, file, range)?;

    let id = walkthrough
        .add_stop(title, narration, focus, before)
        .whatever_context("add the stop")?
        .id
        .clone();

    save(fs, root, &walkthrough, slug)?;
    Ok(id)
}

/// Change the stop `stop`, returning its id.
///
/// The focus is re-captured only when `file` or `range` is given, each falling
/// back to what the stop already holds. An edit that names neither leaves the
/// captured snippet alone, so changing a title never re-baselines a range the
/// author did not mention.
#[allow(clippy::too_many_arguments)]
fn edit_stop(
    fs: &dyn FsHost,
    root: &Path,
    slug: &str,
    stop: &str,
    file: Option<&Path>,
    range: Option<&str>,
    title: Option<Option<String>>,
    narration: Option<String>,
) -> Result<String, Whatever> {
    let mut walkthrough = load(fs, root, slug)?;

    let focus = match (file, range) {
        (None, None) => None,
        _ => {
            let current = &find_stop(&walkthrough, stop)?.focus;
            let file = file.map_or_else(|| root.join(&current.path), Path::to_path_buf);
            let range = match range {
                Some(text) => parse_range(text)?,
                None => RangeSpec::Points(current.range),
            };
            Some(capture_resolved(fs, root, &file, range)?)
        },
    };

    walkthrough
        .edit_stop(
            stop,
            StopEdit {
                title,
                narration,
                focus,
            },
        )
        .whatever_context(format!("edit stop '{stop}'"))?;

    save(fs, root, &walkthrough, slug)?;
    Ok(stop.to_owned())
}

/// Remove the stop `stop` and its annotations, returning its id.
fn remove_stop(fs: &dyn FsHost, root: &Path, slug: &str, stop: &str) -> Result<String, Whatever> {
    let mut walkthrough = load(fs, root, slug)?;
    walkthrough
        .remove_stop(stop)
        .whatever_context(format!("remove stop '{stop}'"))?;

    save(fs, root, &walkthrough, slug)?;
    Ok(stop.to_owned())
}

/// Move the stop `stop` to where `to` names, returning its id.
fn move_stop(
    fs: &dyn FsHost,
    root: &Path,
    slug: &str,
    stop: &str,
    to: &MoveArgs,
) -> Result<String, Whatever> {
    let mut walkthrough = load(fs, root, slug)?;
    walkthrough
        .move_stop(stop, to.target())
        .whatever_context(format!("move stop '{stop}'"))?;

    save(fs, root, &walkthrough, slug)?;
    Ok(stop.to_owned())
}

/// Add an annotation over `range` of `file`, returning its id.
///
/// A `file` of `None` annotates the stop's own focus file, which is the usual
/// case and the one that stores no path of its own.
fn add_annotation(
    fs: &dyn FsHost,
    root: &Path,
    slug: &str,
    stop: &str,
    file: Option<&Path>,
    range: &str,
    label: String,
) -> Result<String, Whatever> {
    let mut walkthrough = load(fs, root, slug)?;

    let target = match file {
        Some(file) => file.to_path_buf(),
        None => root.join(&find_stop(&walkthrough, stop)?.focus.path),
    };
    let captured = capture_resolved(fs, root, &target, parse_range(range)?)?;
    let path = file.is_some().then_some(captured.path);

    let id = walkthrough
        .add_annotation(stop, path, captured.range, captured.snippet, label)
        .whatever_context(format!("annotate stop '{stop}'"))?
        .id
        .clone();

    save(fs, root, &walkthrough, slug)?;
    Ok(id)
}

/// Change the annotation `annotation`, returning its id.
///
/// The snippet is re-captured whenever `file`, `no_file`, or `range` is given,
/// each falling back to what the annotation already holds. A label-only edit
/// leaves the capture alone, so renaming never re-baselines a range the author
/// did not mention.
///
/// A re-capture reads the file the annotation itself names, not the stop's
/// focus, so a bare range edit on a cross-file annotation stays cross-file.
///
/// A file change with no range of its own carries the stored range over as the
/// exact bytes it is, and fails when the new file is too short for it. Name a
/// range alongside the file to move both at once.
#[allow(clippy::too_many_arguments)]
fn edit_annotation(
    fs: &dyn FsHost,
    root: &Path,
    slug: &str,
    stop: &str,
    annotation: &str,
    file: Option<&Path>,
    no_file: bool,
    range: Option<&str>,
    label: Option<String>,
) -> Result<String, Whatever> {
    let mut walkthrough = load(fs, root, slug)?;

    let captured = match (file, no_file, range) {
        (None, false, None) => None,
        _ => {
            let stored = find_annotation(&walkthrough, stop, annotation)?;
            let range = match range {
                Some(text) => parse_range(text)?,
                None => RangeSpec::Points(stored.range),
            };

            let target = match (file, &stored.path) {
                (Some(file), _) => file.to_path_buf(),
                (None, Some(stored)) if !no_file => root.join(stored),
                (None, _) => root.join(&find_stop(&walkthrough, stop)?.focus.path),
            };
            Some(capture_resolved(fs, root, &target, range)?)
        },
    };

    let path = match (file, no_file) {
        (Some(_), _) => captured
            .as_ref()
            .map(|location| Some(location.path.clone())),
        (None, true) => Some(None),
        (None, false) => None,
    };

    walkthrough
        .edit_annotation(
            stop,
            annotation,
            AnnotationEdit {
                path,
                range: captured.as_ref().map(|location| location.range),
                snippet: captured.map(|location| location.snippet),
                label,
            },
        )
        .whatever_context(format!("edit annotation '{annotation}'"))?;

    save(fs, root, &walkthrough, slug)?;
    Ok(annotation.to_owned())
}

/// Remove the annotation `annotation`, returning its id.
fn remove_annotation(
    fs: &dyn FsHost,
    root: &Path,
    slug: &str,
    stop: &str,
    annotation: &str,
) -> Result<String, Whatever> {
    let mut walkthrough = load(fs, root, slug)?;
    walkthrough
        .remove_annotation(stop, annotation)
        .whatever_context(format!("remove annotation '{annotation}'"))?;

    save(fs, root, &walkthrough, slug)?;
    Ok(annotation.to_owned())
}

/// Every range in `root` that no longer reads what it captured, one line each.
///
/// Checks the walkthrough `slug` names, or all of them in slug order when it is
/// `None`. An empty result means every stop still points at what it was written
/// against.
///
/// A line reads `<slug>/<stop>[/<annotation>]: <kind>: <detail>`, where `error`
/// means the file or the range is gone and `stale` means the range still reads
/// but no longer reads the same bytes. The two are worth telling apart: an
/// error needs the range re-pointed, while a stale finding often just needs the
/// snippet re-captured.
fn check(fs: &dyn FsHost, root: &Path, slug: Option<&str>) -> Result<Vec<String>, Whatever> {
    let slugs = match slug {
        Some(slug) => vec![slug.to_owned()],
        None => store::list(fs, root)
            .whatever_context(format!("list the walkthroughs in {}", root.display()))?
            .into_iter()
            .map(|summary| summary.slug)
            .collect(),
    };

    let read = store::workspace_reader(fs, root);
    let mut lines = Vec::new();
    for slug in slugs {
        let walkthrough = load(fs, root, &slug)?;
        lines.extend(
            walkthrough::validate(&walkthrough, &read)
                .iter()
                .map(|finding| finding_line(&slug, finding)),
        );
    }

    Ok(lines)
}

/// One finding as the line `check` prints for it.
fn finding_line(slug: &str, finding: &Finding) -> String {
    let target = match &finding.annotation {
        Some(annotation) => format!("{slug}/{}/{annotation}", finding.stop),
        None => format!("{slug}/{}", finding.stop),
    };
    let kind = match finding.kind {
        FindingKind::Error => "error",
        FindingKind::Stale => "stale",
    };
    format!("{target}: {kind}: {}", finding.detail)
}

/// Print `findings` and fail when there are any.
///
/// The failure exists to make the exit status non-zero, which is what an agent
/// gates on after editing code. It carries only a count, since `main` prints
/// the error on the same stream as the findings, so restating them there prints
/// every line twice.
fn report(findings: Vec<String>) -> Result<(), Whatever> {
    for line in &findings {
        println!("{line}");
    }

    match findings.len() {
        0 => Ok(()),
        1 => whatever!("1 finding"),
        count => whatever!("{count} findings"),
    }
}

/// The location `range` of `file` names, with the bytes it covers right now.
fn capture(fs: &dyn FsHost, root: &Path, file: &Path, range: &str) -> Result<Location, Whatever> {
    capture_resolved(fs, root, file, parse_range(range)?)
}

/// The location `spec` of `file` names, with the bytes it covers right now.
///
/// `file` is canonicalized and required to sit inside the workspace, then
/// stored relative to it. An absolute path in a stored walkthrough breaks the
/// moment the repository is cloned somewhere else.
fn capture_resolved(
    fs: &dyn FsHost,
    root: &Path,
    file: &Path,
    spec: RangeSpec,
) -> Result<Location, Whatever> {
    let relative = within_workspace(fs, root, file)?;
    let content = store::workspace_reader(fs, root)(&relative)
        .with_whatever_context(|| format!("read {}", relative.display()))?;

    let range = resolve_range(spec, &content)?;
    let snippet = walkthrough::snippet_for(&content, range)
        .whatever_context(format!("capture the snippet from {}", relative.display()))?;

    Ok(Location {
        path: relative,
        range,
        snippet,
    })
}

/// `file` as a path relative to the workspace root.
fn within_workspace(fs: &dyn FsHost, root: &Path, file: &Path) -> Result<PathBuf, Whatever> {
    let root = fs
        .canonicalize(root)
        .whatever_context(format!("resolve the workspace {}", root.display()))?;
    let file = fs
        .canonicalize(file)
        .whatever_context(format!("resolve {}", file.display()))?;

    file.strip_prefix(&root)
        .ok()
        .map(Path::to_path_buf)
        .with_whatever_context(|| {
            format!(
                "{} is outside the workspace {}",
                file.display(),
                root.display()
            )
        })
}

/// The stop `stop`, or an error naming the id that is not there.
fn find_stop<'a>(
    walkthrough: &'a Walkthrough,
    stop: &str,
) -> Result<&'a walkthrough::Stop, Whatever> {
    walkthrough
        .stops
        .iter()
        .find(|candidate| candidate.id == stop)
        .with_whatever_context(|| format!("no stop '{stop}'"))
}

/// The annotation `annotation` of stop `stop`, or an error naming the id that
/// is not there.
fn find_annotation<'a>(
    walkthrough: &'a Walkthrough,
    stop: &str,
    annotation: &str,
) -> Result<&'a Annotation, Whatever> {
    find_stop(walkthrough, stop)?
        .annotations
        .iter()
        .find(|candidate| candidate.id == annotation)
        .with_whatever_context(|| format!("no annotation '{annotation}' on stop '{stop}'"))
}

/// Which of `--title` and `--no-title` the caller gave, as the format layer's
/// two-layer edit.
fn title_edit(title: Option<String>, no_title: bool) -> Option<Option<String>> {
    match (title, no_title) {
        (Some(title), _) => Some(Some(title)),
        (None, true) => Some(None),
        (None, false) => None,
    }
}

fn load(fs: &dyn FsHost, root: &Path, slug: &str) -> Result<Walkthrough, Whatever> {
    store::load(fs, root, slug).whatever_context(format!("read walkthrough '{slug}'"))
}

fn save(
    fs: &dyn FsHost,
    root: &Path,
    walkthrough: &Walkthrough,
    slug: &str,
) -> Result<(), Whatever> {
    store::save(fs, root, walkthrough).whatever_context(format!("write walkthrough '{slug}'"))
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
    use super::{
        add_annotation, add_stop, check, delete, edit_annotation, edit_stop, list, move_stop, new,
        parse_range, remove_annotation, remove_stop, show, Annotation, MoveArgs, Point, Range,
        RangeSpec,
    };
    use git2::Repository;
    use std::path::{Path, PathBuf};
    use stoat::{host::LocalFs, walkthrough::store};
    use tempfile::TempDir;

    const SOURCE: &str = "use std::io;\nfn main() {\n    println!(\"hi\");\n}\n";
    const OTHER: &str = "fn helper() {}\nfn second() {}\n";

    /// A workspace holding one source file to focus stops on.
    fn workspace() -> TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        Repository::init(dir.path()).expect("init");
        std::fs::write(dir.path().join("main.rs"), SOURCE).expect("write");
        dir
    }

    /// A walkthrough with one stop over line 2 of `main.rs`.
    fn with_stop(dir: &TempDir) -> PathBuf {
        new(&LocalFs, dir.path(), "tour", None).expect("new");
        add_stop(
            &LocalFs,
            dir.path(),
            "tour",
            &dir.path().join("main.rs"),
            "2",
            None,
            "the entry point".to_owned(),
            None,
        )
        .expect("add-stop");
        dir.path().join("main.rs")
    }

    /// A second file to point annotations at, beside the one stops focus on.
    fn other_file(dir: &TempDir) -> PathBuf {
        let path = dir.path().join("other.rs");
        std::fs::write(&path, OTHER).expect("write");
        path
    }

    fn stored(dir: &TempDir) -> stoat::walkthrough::Walkthrough {
        store::load(&LocalFs, dir.path(), "tour").expect("load")
    }

    /// An annotation as the file it names and the bytes it captured.
    fn annotated(annotation: &Annotation) -> (Option<&str>, &str) {
        (
            annotation.path.as_deref().and_then(Path::to_str),
            annotation.snippet.as_str(),
        )
    }

    fn lines(start: u32, end: u32) -> RangeSpec {
        RangeSpec::Lines { start, end }
    }

    fn points(start: (u32, u32), end: (u32, u32)) -> RangeSpec {
        RangeSpec::Points(Range {
            start: Point {
                line: start.0,
                col: start.1,
            },
            end: Point {
                line: end.0,
                col: end.1,
            },
        })
    }

    #[test]
    fn parse_range_reads_every_accepted_form() {
        assert_eq!(parse_range("3").ok(), Some(lines(3, 3)));
        assert_eq!(parse_range("3-7").ok(), Some(lines(3, 7)));
        assert_eq!(parse_range("3:1-7:12").ok(), Some(points((3, 1), (7, 12))));
    }

    #[test]
    fn parse_range_refuses_what_it_cannot_read() {
        for text in [
            "", "x", "3-", "-7", "3-4:2", "3:1-7", "0", "3:0-4:1", "3:1-4",
        ] {
            assert!(parse_range(text).is_err(), "accepted {text:?}");
        }
    }

    /// A bare line covers the line whole, which needs the file to say where it
    /// ends.
    #[test]
    fn a_bare_line_captures_the_whole_line() {
        let dir = workspace();
        with_stop(&dir);

        let focus = &stored(&dir).stops[0].focus;
        assert_eq!(focus.path, Path::new("main.rs"));
        assert_eq!(focus.snippet, "fn main() {");
        assert_eq!(focus.range.start, Point { line: 2, col: 1 });
        assert_eq!(focus.range.end, Point { line: 2, col: 11 });
    }

    #[test]
    fn add_stop_prints_the_new_id_and_appends() {
        let dir = workspace();
        with_stop(&dir);

        let second = add_stop(
            &LocalFs,
            dir.path(),
            "tour",
            &dir.path().join("main.rs"),
            "1",
            Some("Imports".to_owned()),
            String::new(),
            None,
        )
        .expect("add-stop");

        assert_eq!(second, "s2");
        let walkthrough = stored(&dir);
        assert_eq!(
            walkthrough
                .stops
                .iter()
                .map(|stop| stop.id.as_str())
                .collect::<Vec<_>>(),
            ["s1", "s2"]
        );
        assert_eq!(walkthrough.stops[1].title.as_deref(), Some("Imports"));
    }

    #[test]
    fn add_stop_inserts_before_the_named_stop() {
        let dir = workspace();
        with_stop(&dir);
        add_stop(
            &LocalFs,
            dir.path(),
            "tour",
            &dir.path().join("main.rs"),
            "1",
            None,
            String::new(),
            Some("s1"),
        )
        .expect("add-stop");

        assert_eq!(
            stored(&dir)
                .stops
                .iter()
                .map(|stop| stop.id.as_str())
                .collect::<Vec<_>>(),
            ["s2", "s1"]
        );
    }

    /// A re-baselined snippet the author never mentioned quietly makes a stale
    /// range look current.
    #[test]
    fn a_title_only_edit_leaves_the_snippet_alone() {
        let dir = workspace();
        let file = with_stop(&dir);
        std::fs::write(&file, "CHANGED\nfn main() {\n").expect("rewrite");

        edit_stop(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            None,
            Some(Some("Named".to_owned())),
            None,
        )
        .expect("edit-stop");

        let stop = &stored(&dir).stops[0];
        assert_eq!(stop.title.as_deref(), Some("Named"));
        assert_eq!(
            stop.focus.snippet, "fn main() {",
            "the capture is untouched"
        );
    }

    #[test]
    fn an_edit_naming_a_range_recaptures_the_snippet() {
        let dir = workspace();
        with_stop(&dir);

        edit_stop(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            Some("1"),
            None,
            None,
        )
        .expect("edit-stop");

        assert_eq!(stored(&dir).stops[0].focus.snippet, "use std::io;");
    }

    #[test]
    fn move_stop_reaches_each_destination() {
        let dir = workspace();
        with_stop(&dir);
        for _ in 0..2 {
            add_stop(
                &LocalFs,
                dir.path(),
                "tour",
                &dir.path().join("main.rs"),
                "1",
                None,
                String::new(),
                None,
            )
            .expect("add-stop");
        }

        let ids = |dir: &TempDir| {
            stored(dir)
                .stops
                .iter()
                .map(|stop| stop.id.clone())
                .collect::<Vec<_>>()
        };
        let to = |before: Option<&str>, after: Option<&str>, last: bool| MoveArgs {
            before: before.map(str::to_owned),
            after: after.map(str::to_owned),
            last,
        };

        move_stop(
            &LocalFs,
            dir.path(),
            "tour",
            "s3",
            &to(Some("s1"), None, false),
        )
        .expect("move-stop");
        assert_eq!(ids(&dir), ["s3", "s1", "s2"]);

        move_stop(
            &LocalFs,
            dir.path(),
            "tour",
            "s3",
            &to(None, Some("s1"), false),
        )
        .expect("move-stop");
        assert_eq!(ids(&dir), ["s1", "s3", "s2"]);

        move_stop(&LocalFs, dir.path(), "tour", "s1", &to(None, None, true)).expect("move-stop");
        assert_eq!(ids(&dir), ["s3", "s2", "s1"]);
    }

    #[test]
    fn remove_stop_takes_the_stop_and_its_annotations() {
        let dir = workspace();
        with_stop(&dir);
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            "2:4-2:7",
            "the name".to_owned(),
        )
        .expect("add-annotation");

        assert_eq!(
            remove_stop(&LocalFs, dir.path(), "tour", "s1").expect("remove-stop"),
            "s1"
        );
        assert_eq!(stored(&dir).stops, Vec::new());
    }

    #[test]
    fn an_annotation_captures_from_the_stops_file() {
        let dir = workspace();
        with_stop(&dir);

        let id = add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            "2:4-2:7",
            "the name".to_owned(),
        )
        .expect("add-annotation");

        assert_eq!(id, "a1");
        let annotation = &stored(&dir).stops[0].annotations[0];
        assert_eq!(
            (annotation.snippet.as_str(), annotation.label.as_str()),
            ("main", "the name")
        );
    }

    #[test]
    fn editing_an_annotations_range_recaptures_it() {
        let dir = workspace();
        with_stop(&dir);
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            "2:4-2:7",
            "l".to_owned(),
        )
        .expect("add-annotation");

        edit_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            "a1",
            None,
            false,
            Some("1:5-1:7"),
            Some("moved".to_owned()),
        )
        .expect("edit-annotation");

        let annotation = &stored(&dir).stops[0].annotations[0];
        assert_eq!(
            (annotation.snippet.as_str(), annotation.label.as_str()),
            ("std", "moved")
        );
    }

    #[test]
    fn a_label_only_annotation_edit_leaves_the_snippet_alone() {
        let dir = workspace();
        let file = with_stop(&dir);
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            "2:4-2:7",
            "l".to_owned(),
        )
        .expect("add-annotation");
        std::fs::write(&file, "CHANGED\nCHANGED\n").expect("rewrite");

        edit_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            "a1",
            None,
            false,
            None,
            Some("renamed".to_owned()),
        )
        .expect("edit-annotation");

        let annotation = &stored(&dir).stops[0].annotations[0];
        assert_eq!(
            (annotation.snippet.as_str(), annotation.label.as_str()),
            ("main", "renamed")
        );
    }

    #[test]
    fn an_annotation_may_name_a_file_beside_the_stop() {
        let dir = workspace();
        with_stop(&dir);
        let other = other_file(&dir);

        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            Some(&other),
            "1:4-1:9",
            "the helper".to_owned(),
        )
        .expect("add-annotation");

        let stops = stored(&dir).stops;
        assert_eq!(
            annotated(&stops[0].annotations[0]),
            (Some("other.rs"), "helper"),
        );
        assert_eq!(
            stops[0].focus.path,
            PathBuf::from("main.rs"),
            "annotating elsewhere leaves the stop where it was",
        );
    }

    /// A cross-file annotation reads against its own file, so re-capturing it
    /// must not silently fall back to the stop's focus.
    #[test]
    fn a_range_edit_recaptures_from_the_annotations_own_file() {
        let dir = workspace();
        with_stop(&dir);
        let other = other_file(&dir);
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            Some(&other),
            "1:4-1:9",
            "l".to_owned(),
        )
        .expect("add-annotation");

        edit_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            "a1",
            None,
            false,
            Some("2:4-2:9"),
            None,
        )
        .expect("edit-annotation");

        assert_eq!(
            annotated(&stored(&dir).stops[0].annotations[0]),
            (Some("other.rs"), "second"),
        );
    }

    #[test]
    fn editing_the_file_moves_the_annotation() {
        let dir = workspace();
        with_stop(&dir);
        let other = other_file(&dir);
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            "2:4-2:7",
            "l".to_owned(),
        )
        .expect("add-annotation");

        edit_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            "a1",
            Some(&other),
            false,
            Some("1:4-1:9"),
            None,
        )
        .expect("edit-annotation");

        assert_eq!(
            annotated(&stored(&dir).stops[0].annotations[0]),
            (Some("other.rs"), "helper"),
        );
    }

    /// Clearing the file re-captures without a range of its own, so the stored
    /// range has to be read against the focus file it just returned to.
    #[test]
    fn no_file_returns_the_annotation_to_the_focus() {
        let dir = workspace();
        with_stop(&dir);
        let other = other_file(&dir);
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            Some(&other),
            "1:1-1:3",
            "l".to_owned(),
        )
        .expect("add-annotation");

        edit_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            "a1",
            None,
            true,
            None,
            None,
        )
        .expect("edit-annotation");

        assert_eq!(
            annotated(&stored(&dir).stops[0].annotations[0]),
            (None, "use"),
            "the stored range now reads against main.rs, where it read 'fn '",
        );
    }

    #[test]
    fn show_prints_the_file_an_annotation_names() {
        let dir = workspace();
        with_stop(&dir);
        let other = other_file(&dir);
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            Some(&other),
            "1:4-1:9",
            "l".to_owned(),
        )
        .expect("add-annotation");
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            "2:4-2:7",
            "l".to_owned(),
        )
        .expect("add-annotation");

        let printed = show(&LocalFs, dir.path(), "tour").expect("show");
        assert_eq!(
            printed.matches("\"path\"").count(),
            2,
            "the focus and the cross-file annotation, not the same-file one: {printed}",
        );
        assert!(printed.contains("other.rs"), "got {printed}");
    }

    #[test]
    fn remove_annotation_takes_only_that_annotation() {
        let dir = workspace();
        with_stop(&dir);
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            "1",
            "one".to_owned(),
        )
        .expect("add-annotation");
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            "2",
            "two".to_owned(),
        )
        .expect("add-annotation");

        remove_annotation(&LocalFs, dir.path(), "tour", "s1", "a1").expect("remove-annotation");

        let annotations = &stored(&dir).stops[0].annotations;
        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].id, "a2");
    }

    /// A stored path has to survive the repository being cloned elsewhere, so a
    /// file outside the workspace has no form that still resolves.
    #[test]
    fn a_file_outside_the_workspace_is_refused() {
        let dir = workspace();
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::write(outside.path().join("other.rs"), SOURCE).expect("write");
        new(&LocalFs, dir.path(), "tour", None).expect("new");

        assert!(add_stop(
            &LocalFs,
            dir.path(),
            "tour",
            &outside.path().join("other.rs"),
            "1",
            None,
            String::new(),
            None,
        )
        .is_err());
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

    #[test]
    fn check_says_nothing_about_a_walkthrough_that_still_matches() {
        let dir = workspace();
        with_stop(&dir);
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            "1",
            "l".to_owned(),
        )
        .expect("add-annotation");

        assert_eq!(check(&LocalFs, dir.path(), None).expect("check"), [""; 0]);
    }

    /// The whole point of capturing snippets. A range that still resolves but
    /// covers different bytes is drift, and nothing else reports it.
    #[test]
    fn check_reports_an_edited_file_as_stale() {
        let dir = workspace();
        let file = with_stop(&dir);
        std::fs::write(&file, "use std::io;\nfn MAIN() {\n").expect("rewrite");

        let found = check(&LocalFs, dir.path(), None).expect("check");
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(
            found[0].starts_with("tour/s1: stale: "),
            "got {:?}",
            found[0]
        );
        assert!(
            found[0].contains("fn main() {") && found[0].contains("fn MAIN() {"),
            "the line shows what was captured and what is there: {:?}",
            found[0],
        );
    }

    #[test]
    fn check_reports_a_deleted_file_as_an_error() {
        let dir = workspace();
        let file = with_stop(&dir);
        std::fs::remove_file(&file).expect("remove");

        let found = check(&LocalFs, dir.path(), None).expect("check");
        assert_eq!(found, ["tour/s1: error: cannot read main.rs"]);
    }

    #[test]
    fn check_names_the_annotation_that_drifted() {
        let dir = workspace();
        let file = with_stop(&dir);
        add_annotation(
            &LocalFs,
            dir.path(),
            "tour",
            "s1",
            None,
            "1",
            "l".to_owned(),
        )
        .expect("add-annotation");
        // Line 2 stays put, so only the annotation over line 1 drifts.
        std::fs::write(&file, "USE STD::IO;\nfn main() {\n").expect("rewrite");

        let found = check(&LocalFs, dir.path(), None).expect("check");
        assert_eq!(found.len(), 1, "got {found:?}");
        assert!(
            found[0].starts_with("tour/s1/a1: stale: "),
            "an annotation finding names it after its stop: {:?}",
            found[0],
        );
    }

    /// An agent checking one tour must not be failed by another one's drift.
    #[test]
    fn checking_one_slug_ignores_the_others() {
        let dir = workspace();
        let file = with_stop(&dir);
        new(&LocalFs, dir.path(), "other", None).expect("new");
        add_stop(
            &LocalFs,
            dir.path(),
            "other",
            &file,
            "1",
            None,
            String::new(),
            None,
        )
        .expect("add-stop");
        std::fs::write(&file, "USE STD::IO;\nfn MAIN() {\n").expect("rewrite");

        assert_eq!(
            check(&LocalFs, dir.path(), Some("tour"))
                .expect("check")
                .len(),
            1,
            "only the named walkthrough is checked",
        );
        assert_eq!(
            check(&LocalFs, dir.path(), None).expect("check").len(),
            2,
            "and a bare check covers both",
        );
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

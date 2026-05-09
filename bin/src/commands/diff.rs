use clap::Args;
use snafu::{whatever, ResultExt, Whatever};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{
    diff::{extract_review_hunks_changeset, ReviewFileInput, ReviewHunk, ReviewRow},
    host::{FsHost, LocalFs},
};

#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Left side of the diff (the "before" version).
    pub left: PathBuf,
    /// Right side of the diff (the "after" version).
    pub right: PathBuf,

    /// Run as a `git diff` external tool, expecting the
    /// `GIT_EXTERNAL_DIFF` calling convention. Not yet implemented.
    #[arg(long)]
    pub git: bool,

    /// Render the diff in a two-column side-by-side layout.
    #[arg(long)]
    pub side_by_side: bool,

    /// Render the diff in a single-column unified layout.
    #[arg(long)]
    pub unified: bool,

    /// Disable ANSI color escapes.
    #[arg(long)]
    pub no_color: bool,

    /// Skip piping output through `$PAGER`.
    #[arg(long)]
    pub no_pager: bool,

    /// Override the auto-detected terminal width.
    #[arg(long)]
    pub width: Option<u16>,

    /// Force the language used for tokenization.
    #[arg(long)]
    pub language: Option<String>,
}

pub fn run(args: DiffArgs) -> Result<(), Whatever> {
    if args.git {
        // FIXME: --git mode lands with the GIT_EXTERNAL_DIFF child of the
        // `stoat diff` TODO; it needs language detection via stoat_language.
        whatever!("--git mode is not yet implemented");
    }

    // FIXME: --side-by-side / --unified / --no-color / --no-pager / --width /
    // --language are parsed but not yet threaded; the renderer + pager
    // children of the `stoat diff` TODO consume them.

    let fs: Arc<dyn FsHost> = Arc::new(LocalFs);
    let base_text = read_utf8(&*fs, &args.left)?;
    let buffer_text = read_utf8(&*fs, &args.right)?;

    let rel_path = path_label(&args.right);
    let inputs = vec![ReviewFileInput {
        path: args.right.clone(),
        rel_path,
        language: None,
        base_text: Arc::new(base_text),
        buffer_text: Arc::new(buffer_text),
    }];

    let per_file = extract_review_hunks_changeset(&inputs, 3);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (input, hunks) in inputs.iter().zip(per_file.iter()) {
        write_file_dump(&mut out, &input.rel_path, hunks)
            .whatever_context("write diff to stdout")?;
    }
    Ok(())
}

fn read_utf8(fs: &dyn FsHost, path: &Path) -> Result<String, Whatever> {
    let mut buf = Vec::new();
    fs.read(path, &mut buf)
        .whatever_context(format!("read {}", path.display()))?;
    String::from_utf8(buf).whatever_context(format!("{} is not valid UTF-8", path.display()))
}

fn path_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn write_file_dump(
    out: &mut dyn Write,
    rel_path: &str,
    hunks: &[ReviewHunk],
) -> std::io::Result<()> {
    if hunks.is_empty() {
        return Ok(());
    }
    writeln!(out, "--- {rel_path}")?;
    for hunk in hunks {
        writeln!(out, "@@")?;
        for row in &hunk.rows {
            match row {
                ReviewRow::Context { left, .. } => writeln!(out, " {}", left.text)?,
                ReviewRow::Changed { left, right } => {
                    if let Some(l) = left {
                        writeln!(out, "-{}", l.text)?;
                    }
                    if let Some(r) = right {
                        writeln!(out, "+{}", r.text)?;
                    }
                },
            }
        }
    }
    Ok(())
}

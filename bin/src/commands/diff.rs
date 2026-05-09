use clap::Args;
use snafu::{whatever, ResultExt, Whatever};
use std::{
    io::{self, IsTerminal, Write},
    process::{Command, Stdio},
    sync::Arc,
};
use stoat::{
    diff::{extract_review_hunks_changeset, scan_working_tree, ReviewFileInput, ReviewHunk},
    diff_render_cli::{
        detect_color_enabled, detect_width, render_diff, CliLayout, CliRenderOptions,
    },
    host::{FsHost, GitHost, LocalFs, LocalGit},
};
use stoat_language::LanguageRegistry;

#[derive(Args, Debug)]
pub struct DiffArgs {
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
        // `stoat diff` TODO.
        whatever!("--git mode is not yet implemented");
    }

    // FIXME: --language is parsed but not threaded; landing alongside the
    // --git child of the `stoat diff` TODO.

    let cwd = std::env::current_dir().whatever_context("read current directory")?;
    let fs: Arc<dyn FsHost> = Arc::new(LocalFs);
    let git: Arc<dyn GitHost> = Arc::new(LocalGit::new());
    let langs = LanguageRegistry::standard();

    let Some((_workdir, inputs)) = scan_working_tree(&*git, &*fs, &langs, &cwd) else {
        return Ok(());
    };

    let per_file = extract_review_hunks_changeset(&inputs, 3);

    let opts = CliRenderOptions {
        layout: if args.unified {
            CliLayout::Unified
        } else {
            CliLayout::SideBySide
        },
        width: args.width.unwrap_or_else(detect_width),
        color: detect_color_enabled(args.no_color),
    };

    match select_pager(args.no_pager) {
        Some(argv) => render_via_pager(&argv, &inputs, &per_file, &opts),
        None => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            render_all(&mut out, &inputs, &per_file, &opts).whatever_context("write diff to stdout")
        },
    }
}

fn render_all<W: Write>(
    out: &mut W,
    inputs: &[ReviewFileInput],
    per_file: &[Vec<ReviewHunk>],
    opts: &CliRenderOptions,
) -> io::Result<()> {
    for (input, hunks) in inputs.iter().zip(per_file.iter()) {
        render_diff(out, &input.rel_path, hunks, opts)?;
    }
    Ok(())
}

fn render_via_pager(
    argv: &[String],
    inputs: &[ReviewFileInput],
    per_file: &[Vec<ReviewHunk>],
    opts: &CliRenderOptions,
) -> Result<(), Whatever> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .spawn()
        .whatever_context(format!("spawn pager `{}`", argv.join(" ")))?;
    let mut stdin = child.stdin.take().expect("stdin set to piped above");

    let render_result = render_all(&mut stdin, inputs, per_file, opts);
    drop(stdin);
    let _ = child.wait();

    match render_result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e).whatever_context("write diff to pager"),
    }
}

/// Resolve which pager (if any) should consume stdout. `None` means
/// write directly to stdout.
fn select_pager(no_pager_flag: bool) -> Option<Vec<String>> {
    if no_pager_flag {
        return None;
    }
    if !io::stdout().is_terminal() {
        return None;
    }
    let cmd = match std::env::var("STOAT_PAGER") {
        Ok(v) if v.is_empty() => return None,
        Ok(v) => v,
        Err(_) => "less -RFX".to_string(),
    };
    let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
    if argv.is_empty() {
        return None;
    }
    Some(argv)
}

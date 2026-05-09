use clap::Args;
use snafu::{whatever, ResultExt, Whatever};
use std::sync::Arc;
use stoat::{
    diff::{extract_review_hunks_changeset, scan_working_tree},
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
    // --git child of the `stoat diff` TODO. --no-pager is parsed but not
    // honoured; the pager child handles it.

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

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (input, hunks) in inputs.iter().zip(per_file.iter()) {
        render_diff(&mut out, &input.rel_path, hunks, &opts)
            .whatever_context("write diff to stdout")?;
    }
    Ok(())
}

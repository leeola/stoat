use clap::Args;
use snafu::{whatever, FromString, ResultExt, Whatever};
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
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
    /// `GIT_EXTERNAL_DIFF` 7-arg calling convention:
    /// `path old-file old-hex old-mode new-file new-hex new-mode`.
    /// Either `old-file` or `new-file` may be `/dev/null` for
    /// added or deleted files.
    ///
    /// Wire it up in `~/.gitconfig`:
    ///
    /// ```text
    /// [diff]
    ///     external = "stoat diff --git"
    /// ```
    ///
    /// or per-invocation: `GIT_EXTERNAL_DIFF="stoat diff --git" git diff`.
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

    /// Positional args. Required (exactly seven) with `--git`,
    /// where they supply the `GIT_EXTERNAL_DIFF` parameters in
    /// order. Rejected in the default (workspace-scan) mode.
    pub git_args: Vec<String>,
}

pub fn run(args: DiffArgs) -> Result<(), Whatever> {
    // FIXME: --language is parsed but not threaded; per-file detection
    // via `LanguageRegistry::for_path` covers default and --git modes.

    let fs: Arc<dyn FsHost> = Arc::new(LocalFs);
    let git: Arc<dyn GitHost> = Arc::new(LocalGit::new());
    let cwd = std::env::current_dir().whatever_context("read current directory")?;

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
        Some(argv) => {
            let mut child = Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::piped())
                .spawn()
                .whatever_context(format!("spawn pager `{}`", argv.join(" ")))?;
            let mut stdin = child.stdin.take().expect("stdin set to piped above");
            let render_result = run_with_io(&args, &*fs, &*git, &cwd, &opts, &mut stdin);
            drop(stdin);
            let _ = child.wait();
            match render_result {
                Ok(()) => Ok(()),
                Err(WriteError::BrokenPipe) => Ok(()),
                Err(WriteError::Other(e)) => Err(e),
            }
        },
        None => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            run_with_io(&args, &*fs, &*git, &cwd, &opts, &mut out)
                .map_err(WriteError::into_whatever)
        },
    }
}

/// Testable core of [`run`]. Caller resolves environment-driven
/// inputs (cwd, color/width, pager wrapping) and supplies the
/// renderer sink.
pub fn run_with_io<W: Write>(
    args: &DiffArgs,
    fs: &dyn FsHost,
    git: &dyn GitHost,
    cwd: &Path,
    opts: &CliRenderOptions,
    out: &mut W,
) -> Result<(), WriteError> {
    let langs = LanguageRegistry::standard();

    let inputs = if args.git {
        match read_git_external_inputs(fs, &langs, &args.git_args) {
            Ok(input) => vec![input],
            Err(e) => return Err(WriteError::Other(e)),
        }
    } else {
        if !args.git_args.is_empty() {
            return Err(WriteError::Other(FromString::without_source(format!(
                "positional args are only valid with --git; got {} unexpected arg(s)",
                args.git_args.len()
            ))));
        }
        let Some((_workdir, inputs)) = scan_working_tree(git, fs, &langs, cwd) else {
            return Ok(());
        };
        inputs
    };

    let per_file = extract_review_hunks_changeset(&inputs, 3);

    render_all(out, &inputs, &per_file, opts).map_err(|e| {
        if e.kind() == io::ErrorKind::BrokenPipe {
            WriteError::BrokenPipe
        } else {
            WriteError::Other(FromString::without_source(format!("write diff: {e}")))
        }
    })
}

/// Distinguishes a benign broken-pipe write (pager quit early)
/// from any other failure.
#[derive(Debug)]
pub enum WriteError {
    BrokenPipe,
    Other(Whatever),
}

impl WriteError {
    fn into_whatever(self) -> Whatever {
        match self {
            WriteError::BrokenPipe => FromString::without_source(
                "diff write closed early before output completed".to_string(),
            ),
            WriteError::Other(e) => e,
        }
    }
}

/// Parse the seven `GIT_EXTERNAL_DIFF` positional args into a single
/// [`ReviewFileInput`]. `/dev/null` on either side means create or
/// delete; that side becomes empty text and the structural-diff
/// pipeline emits the appropriate one-sided rows.
fn read_git_external_inputs(
    fs: &dyn FsHost,
    langs: &LanguageRegistry,
    git_args: &[String],
) -> Result<ReviewFileInput, Whatever> {
    if git_args.len() != 7 {
        whatever!(
            "--git mode requires exactly 7 positional args (path old-file old-hex old-mode \
             new-file new-hex new-mode); got {}",
            git_args.len()
        );
    }
    let [path, old_file, _old_hex, _old_mode, new_file, _new_hex, _new_mode]: &[String; 7] =
        git_args
            .try_into()
            .expect("len checked to be 7 immediately above");

    let logical = PathBuf::from(path);
    let base_text = read_blob_text(fs, old_file)?;
    let buffer_text = read_blob_text(fs, new_file)?;
    let language = langs.for_path(&logical);

    Ok(ReviewFileInput {
        rel_path: path.clone(),
        path: logical,
        language,
        base_text: Arc::new(base_text),
        buffer_text: Arc::new(buffer_text),
    })
}

fn read_blob_text(fs: &dyn FsHost, path: &str) -> Result<String, Whatever> {
    if path == "/dev/null" {
        return Ok(String::new());
    }
    let p = Path::new(path);
    let mut buf = Vec::new();
    fs.read(p, &mut buf)
        .whatever_context(format!("read {path}"))?;
    String::from_utf8(buf).whatever_context(format!("{path} is not valid UTF-8"))
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

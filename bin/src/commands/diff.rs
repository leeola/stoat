use clap::Args;
use futures::stream::{FuturesUnordered, StreamExt};
use snafu::{whatever, FromString, ResultExt, Whatever};
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};
use stoat::{
    diff::{extract_review_hunks_changeset, scan_working_tree, ReviewFileInput, ReviewHunk},
    diff_cache::deserialize_hunks,
    diff_render_cli::{
        detect_color_enabled, detect_width, render_diff, CliLayout, CliRenderOptions,
    },
    host::{FsHost, GitHost, LocalFs, LocalGit},
};
use stoat_language::LanguageRegistry;
use viewport::{
    protocol::{ToMain, ToViewport},
    ViewportClient,
};

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
    let socket_dir = runtime_socket_dir();

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
            let render_result = run_with_io(
                &args,
                &*fs,
                &*git,
                &cwd,
                Some(&socket_dir),
                &opts,
                &mut stdin,
            );
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
            run_with_io(&args, &*fs, &*git, &cwd, Some(&socket_dir), &opts, &mut out)
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
    socket_dir: Option<&Path>,
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

    let per_file = cached_hunks_via_socket(socket_dir, &inputs)
        .unwrap_or_else(|| extract_review_hunks_changeset(&inputs, 3));

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

/// Runtime directory the editor's viewport socket lives in.
/// Production reads `$TMPDIR` on macOS and `$XDG_RUNTIME_DIR`
/// elsewhere with a `/tmp` fallback; tests pass an explicit
/// `socket_dir` to [`run_with_io`].
fn runtime_socket_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
    } else {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"))
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

/// Probe a running editor's diff cache before computing the
/// structural diff inline. Returns `Some(per_file_hunks)` only
/// when every input is a cache hit; any miss / connect failure /
/// timeout returns `None` so the caller falls back silently to
/// inline computation.
///
/// `socket_dir` is the directory to scan for `stoat-*.sock` files.
/// `None` skips cache discovery entirely (used in tests that don't
/// want to touch the filesystem).
fn cached_hunks_via_socket(
    socket_dir: Option<&Path>,
    inputs: &[ReviewFileInput],
) -> Option<Vec<Vec<ReviewHunk>>> {
    let candidates = discover_sockets(socket_dir?);
    if candidates.is_empty() {
        return None;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async {
        // Probe every candidate at once so one dead session's timeout does not
        // serialize in front of a live editor. The first useful reply wins.
        let mut probes: FuturesUnordered<_> = candidates
            .iter()
            .map(|sock| {
                tokio::time::timeout(
                    Duration::from_millis(250),
                    fetch_all_from_socket(sock, inputs),
                )
            })
            .collect();
        while let Some(attempt) = probes.next().await {
            if let Ok(Some(hunks)) = attempt {
                return Some(hunks);
            }
        }
        None
    })
}

async fn fetch_all_from_socket(
    sock: &Path,
    inputs: &[ReviewFileInput],
) -> Option<Vec<Vec<ReviewHunk>>> {
    let mut client = match ViewportClient::connect(sock).await {
        Ok(client) => client,
        Err(e) => {
            // A leftover socket from a dead session refuses the connection, or
            // the file vanished between discovery and connect. Unlink it so
            // later runs do not probe it again.
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) {
                let _ = std::fs::remove_file(sock);
            }
            return None;
        },
    };
    let mut results = Vec::with_capacity(inputs.len());
    for input in inputs {
        let req = ToMain::DiffRequest {
            left_hash: blake3::hash(input.base_text.as_bytes()).into(),
            right_hash: blake3::hash(input.buffer_text.as_bytes()).into(),
            language: input.language.as_ref().map(|l| l.name.to_string()),
        };
        client.send(req).await.ok()?;
        let bytes = match client.recv().await.ok()?? {
            ToViewport::DiffResponse {
                result: Some(bytes),
            } => bytes,
            _ => return None,
        };
        results.push(deserialize_hunks(&bytes).ok()?);
    }
    Some(results)
}

/// List Unix-socket files in `dir` that look like stoat editor
/// sessions. Empty when `dir` is missing or unreadable.
fn discover_sockets(dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return Vec::new(),
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("stoat-") && n.ends_with(".sock"))
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> ReviewFileInput {
        ReviewFileInput {
            rel_path: name.to_string(),
            path: PathBuf::from(name),
            language: None,
            base_text: Arc::new(format!("{name} base\n")),
            buffer_text: Arc::new(format!("{name} buffer\n")),
        }
    }

    #[test]
    fn a_stale_socket_is_unlinked() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("stoat-stale.sock");
        // Bind then drop, leaving the socket file behind with no listener.
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        assert!(stale.exists());

        let inputs = [input("a.rs")];
        let result = cached_hunks_via_socket(Some(dir.path()), &inputs);

        assert!(result.is_none(), "a stale socket yields no cache hit");
        assert!(!stale.exists(), "the stale socket is unlinked");
    }

    #[test]
    fn a_live_editor_answers_past_a_stale_socket() {
        use bytes::Bytes;
        use futures::SinkExt;
        use stoat::diff_cache::serialize_hunks;
        use tokio_util::codec::{FramedRead, FramedWrite};
        use viewport::protocol::{ToMainCodec, ToViewportCodec};

        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("stoat-stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        let live = dir.path().join("stoat-live.sock");

        // A live editor that answers each DiffRequest with two empty hunks,
        // running on its own runtime so it accepts while the probe blocks.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let responder = std::thread::spawn({
            let live = live.clone();
            move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let listener = tokio::net::UnixListener::bind(&live).unwrap();
                    ready_tx.send(()).unwrap();
                    let (stream, _) = listener.accept().await.unwrap();
                    let (r, w) = stream.into_split();
                    let mut reader = FramedRead::new(r, ToMainCodec::new());
                    let mut writer = FramedWrite::new(w, ToViewportCodec::new());
                    if let Some(Ok(ToMain::DiffRequest { .. })) = reader.next().await {
                        let bytes = serialize_hunks(&[
                            ReviewHunk { rows: Vec::new() },
                            ReviewHunk { rows: Vec::new() },
                        ]);
                        writer
                            .send(ToViewport::DiffResponse {
                                result: Some(Bytes::from(bytes)),
                            })
                            .await
                            .unwrap();
                    }
                });
            }
        });
        ready_rx.recv().unwrap();

        let inputs = [input("a.rs")];
        let per_file = cached_hunks_via_socket(Some(dir.path()), &inputs)
            .expect("the live editor answers past the stale socket");

        assert_eq!(per_file.len(), 1, "one file's hunks came back");
        assert_eq!(
            per_file[0].len(),
            2,
            "the live editor's two hunks came back"
        );
        assert!(!stale.exists(), "the stale socket was unlinked");

        responder.join().unwrap();
    }
}

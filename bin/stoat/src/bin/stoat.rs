//! Process entry point for the `stoat` editor.
//!
//! This is the syscall boundary the host layer exists to keep out of everything
//! below it. Logging has to be configured and its directory created before any
//! host is constructed, so the reads here are the intended implementation
//! rather than a bypass.
#![allow(clippy::disallowed_methods)]

use clap::Parser;
#[cfg(unix)]
use std::io::IsTerminal;
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use stoat::log::ident::{self, LogId, ProcessIdent};
use stoat_bin::commands::term_open::{self, Forward};

/// The log stem every client invocation shares, where a session takes a stem
/// of its own.
const CLIENT_LOG_STEM: &str = "stoat-cli";

/// The size past which the shared client log moves aside to `.log.old`.
///
/// Nothing else prunes the file every client appends to, so this bounds it at
/// twice the cap.
const CLIENT_LOG_MAX: u64 = 4 * 1024 * 1024;

fn main() {
    let args = stoat_bin::commands::default::Args::parse();

    // A bare file open from a stoat terminal pane goes to the parent instance
    // and exits. Trying it before logging starts leaves no log file behind.
    // Outside a pane this costs one absent environment read.
    let forward_failure = match args.forwardable_files().map(term_open::try_forward) {
        Some(Forward::Opened) => return,
        Some(Forward::Failed(reason)) => Some(reason),
        Some(Forward::NoParent) | None => None,
    };

    let stoat_log = std::env::var("STOAT_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();

    let stoatty_id = std::env::var("STOATTY_LOG_ID").ok();
    let id = LogId::mint();
    ident::install(ProcessIdent {
        file_stem: log_file_stem(stoatty_id.as_deref(), &id, args.session_log()),
        id,
    });
    let installed = ident::get().expect("ident installed above");

    let log_path = if args.log_stderr {
        None
    } else {
        match resolve_log_path(&installed.file_stem) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("Failed to prepare log directory: {e}");
                std::process::exit(1);
            },
        }
    };
    let target = match &log_path {
        Some(p) => stoat::log::LogTarget::File(p.clone()),
        None => stoat::log::LogTarget::Stderr,
    };
    if let Err(e) = stoat::log::init(stoat_log, rust_log, target) {
        eprintln!("Failed to initialize logging: {e}");
        std::process::exit(1);
    }

    // A terminal-bound stderr lets third-party crates that write raw bytes to
    // fd 2 (arboard's debug drop-warning) paint over the raw-mode TUI. Send fd 2
    // to the log instead. The is_terminal guard preserves an explicit `2>file`,
    // and --log-stderr keeps its console output by leaving log_path None.
    #[cfg(unix)]
    if let Some(path) = &log_path
        && io::stderr().is_terminal()
    {
        redirect_stderr_to(path);
    }

    tracing::info!(
        log_id = %installed.id,
        stoatty_log_id = ?stoatty_id,
        hostname = %ident::hostname(),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        cpus = std::thread::available_parallelism().map_or(0, |n| n.get()),
        term = ?std::env::var("TERM").ok(),
        colorterm = ?std::env::var("COLORTERM").ok(),
        stoatty = std::env::var_os("STOATTY").is_some(),
        "Starting Stoat editor"
    );
    if let Some(reason) = forward_failure {
        tracing::warn!(
            target: "stoat::bin",
            %reason,
            "the parent instance did not take the files; starting a nested session",
        );
    }

    if let Err(e) = stoat_bin::commands::default::run(args) {
        println!("Error: {e}");
        std::process::exit(1);
    }
}

fn resolve_log_path(stem: &str) -> io::Result<PathBuf> {
    log_path_in(&stoat::log::log_dir()?, stem)
}

/// The log file for `stem` in `dir`, which this creates when it does not exist.
///
/// The shared client log moves aside to `stoat-cli.log.old` once it passes
/// [`CLIENT_LOG_MAX`], replacing the one there. A session's log belongs to that
/// session alone and never moves.
fn log_path_in(dir: &Path, stem: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{stem}.log"));
    if stem == CLIENT_LOG_STEM && fs::metadata(&path).is_ok_and(|meta| meta.len() > CLIENT_LOG_MAX)
    {
        // A client that rotated first leaves nothing to rename, and a failed
        // rename only lets the file grow, so neither stops the command.
        let _ = fs::rename(&path, dir.join(format!("{stem}.log.old")));
    }
    Ok(path)
}

/// The log filename stem for this stoat process, minus the `.log` extension.
///
/// A client, which runs no `session`, logs under [`CLIENT_LOG_STEM`] wherever
/// it runs. When a session runs inside stoatty, `stoatty_id` is stoatty's log
/// id (from `STOATTY_LOG_ID`), so the stem is `stoatty-<sid>-stoat-<id>` and the
/// file sorts next to the stoatty log. Without it (ssh, a foreign terminal), the
/// stem is `headless-stoat-<id>`.
fn log_file_stem(stoatty_id: Option<&str>, id: &LogId, session: bool) -> String {
    if !session {
        return CLIENT_LOG_STEM.to_owned();
    }
    match stoatty_id {
        Some(sid) => format!("stoatty-{sid}-stoat-{id}"),
        None => format!("headless-stoat-{id}"),
    }
}

/// Point fd 2 at the log file so crates that write raw bytes to stderr
/// (arboard's debug-build Drop warning is the known case) land in the log
/// rather than painting over the raw-mode TUI. Best-effort: an open or `dup2`
/// failure is warn-logged and the original stderr left in place.
#[cfg(unix)]
fn redirect_stderr_to(path: &Path) {
    use std::os::fd::AsRawFd;

    let file = match fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!("failed to open log file for stderr redirect: {e}");
            return;
        },
    };

    // SAFETY: dup2 takes two descriptors by value, and `file` is open for the
    // call. It installs the file's open description onto fd 2, so the File can
    // then drop and fd 2 keeps that description alive.
    if unsafe { libc::dup2(file.as_raw_fd(), 2) } == -1 {
        tracing::warn!(
            error = %io::Error::last_os_error(),
            "failed to redirect stderr to log"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn log_file_stem_prefixes_with_stoatty_id_when_present() {
        let id = LogId::new(datetime!(2026-07-18 14:30:22 UTC), 12345);
        assert_eq!(
            log_file_stem(Some("20260718-143000-99"), &id, true),
            "stoatty-20260718-143000-99-stoat-20260718-143022-12345"
        );
        assert_eq!(
            log_file_stem(None, &id, true),
            "headless-stoat-20260718-143022-12345"
        );
        assert_eq!(
            [
                log_file_stem(Some("20260718-143000-99"), &id, false),
                log_file_stem(None, &id, false),
            ],
            [CLIENT_LOG_STEM; 2],
            "a client shares one stem wherever it runs"
        );
    }

    #[test]
    fn only_a_client_log_past_its_cap_moves_aside() {
        let session = "headless-stoat-20260718-143022-12345";
        let cases = [
            (CLIENT_LOG_STEM, CLIENT_LOG_MAX + 1, (false, true)),
            (CLIENT_LOG_STEM, CLIENT_LOG_MAX, (true, false)),
            (session, CLIENT_LOG_MAX + 1, (true, false)),
        ];
        // Each case ends as (stem, size, (log exists, old exists)).
        let outcomes: Vec<(&str, u64, (bool, bool))> = cases
            .iter()
            .map(|&(stem, len, _)| {
                let dir = tempfile::tempdir().expect("tempdir");
                let log = dir.path().join(format!("{stem}.log"));
                fs::File::create(&log)
                    .and_then(|file| file.set_len(len))
                    .expect("seed the log");

                assert_eq!(log_path_in(dir.path(), stem).expect("resolve"), log);
                let old = dir.path().join(format!("{stem}.log.old"));
                (stem, len, (log.exists(), old.exists()))
            })
            .collect();
        assert_eq!(outcomes, cases);
    }
}

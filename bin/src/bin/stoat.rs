use clap::Parser;
#[cfg(unix)]
use std::io::IsTerminal;
use stoat::log::ident::{self, LogId, ProcessIdent};

fn main() {
    let args = stoat_bin::commands::default::Args::parse();
    let stoat_log = std::env::var("STOAT_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();

    let stoatty_id = std::env::var("STOATTY_LOG_ID").ok();
    let id = LogId::mint();
    ident::install(ProcessIdent {
        file_stem: log_file_stem(stoatty_id.as_deref(), &id),
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
        && std::io::stderr().is_terminal()
    {
        redirect_stderr_to(path);
    }

    tracing::info!(
        log_id = %installed.id,
        stoatty_log_id = ?stoatty_id,
        hostname = %ident::hostname(),
        "Starting Stoat editor"
    );

    if let Err(e) = stoat_bin::commands::default::run(args) {
        println!("Error: {e}");
        std::process::exit(1);
    }
}

fn resolve_log_path(stem: &str) -> std::io::Result<std::path::PathBuf> {
    let dir = stoat::log::log_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{stem}.log")))
}

/// The log filename stem for this stoat process, minus the `.log` extension.
///
/// When stoat runs inside stoatty, `stoatty_id` is stoatty's log id (from
/// `STOATTY_LOG_ID`), so the stem is `stoatty-<sid>-stoat-<id>` and the file
/// sorts next to the stoatty log. Without it (ssh, a foreign terminal), the stem
/// is `headless-stoat-<id>`.
fn log_file_stem(stoatty_id: Option<&str>, id: &LogId) -> String {
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
fn redirect_stderr_to(path: &std::path::Path) {
    use std::os::fd::AsRawFd;

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!("failed to open log file for stderr redirect: {e}");
            return;
        },
    };

    // dup2 installs the file's open description onto fd 2. The File can then
    // drop, since fd 2 keeps that description alive.
    if unsafe { libc::dup2(file.as_raw_fd(), 2) } == -1 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
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
            log_file_stem(Some("20260718-143000-99"), &id),
            "stoatty-20260718-143000-99-stoat-20260718-143022-12345"
        );
        assert_eq!(
            log_file_stem(None, &id),
            "headless-stoat-20260718-143022-12345"
        );
    }
}

//! Binary entry point for the `stoatty` terminal: parses argv, opens a window
//! running the requested command (or the user's shell), and drives the event
//! loop until the window closes.

use clap::{CommandFactory, Parser};
use std::{backtrace::Backtrace, panic, sync::Once};
use stoat_cli::FixtureSub;
use stoat_log::LogTarget;
use stoatty::cli::{Cli, TtyCommand};

fn main() {
    let mut cli = Cli::parse();

    let stoat_log_env = std::env::var("STOAT_LOG").ok();
    let rust_log_env = std::env::var("RUST_LOG").ok();
    let target = match resolve_log_path() {
        Ok(path) => LogTarget::File(path),
        Err(e) => {
            eprintln!("Failed to prepare log directory: {e}");
            std::process::exit(1);
        },
    };
    if let Err(e) = stoat_log::init(stoat_log_env, rust_log_env, target) {
        eprintln!("Failed to initialize logging: {e}");
        std::process::exit(1);
    }
    install_panic_hook();
    tracing::info!("starting stoatty");

    match cli.command_sub.take() {
        Some(TtyCommand::Fixture(args)) => match (args.sub, args.name) {
            (Some(FixtureSub::Ls), _) => {
                print!("{}", stoat_cli::ls_text());
                return;
            },
            (None, Some(name)) => {
                if cli.common.fixture.is_some() {
                    eprintln!("error: `--fixture` conflicts with the fixture subcommand");
                    std::process::exit(1);
                }
                cli.common.fixture = Some(name);
            },
            (None, None) => {
                eprintln!("error: specify a fixture name or `ls`");
                std::process::exit(1);
            },
        },
        Some(TtyCommand::Completions { shell }) => {
            clap_complete::generate(
                shell,
                &mut <Cli as CommandFactory>::command(),
                "stoatty",
                &mut std::io::stdout(),
            );
            return;
        },
        None => {},
    }

    stoatty::app::run(
        cli.command(),
        cli.working_directory,
        cli.common,
        cli.terminal,
    );
}

/// The per-process log file under the shared stoat log directory, named
/// `stoatty-<pid>.log` to sit beside the editor's `stoat-<pid>.log`. Creates the
/// log directory if it does not yet exist.
fn resolve_log_path() -> std::io::Result<std::path::PathBuf> {
    let dir = stoat_log::log_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("stoatty-{}.log", std::process::id())))
}

/// Install a process-global panic hook that records the panic message,
/// location, and a captured backtrace via [`tracing::error`] before delegating
/// to the prior hook, so a panic survives in `stoatty-<pid>.log` after the
/// window is gone. Idempotent across repeated calls.
///
/// Unlike the editor's hook, stoatty runs a GUI rather than a raw-mode terminal,
/// so there is no cooked-mode restore to perform here.
fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let prior = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let panic_message = match info.payload().downcast_ref::<&'static str>() {
                Some(message) => *message,
                None => match info.payload().downcast_ref::<String>() {
                    Some(message) => message.as_str(),
                    None => "Box<Any>",
                },
            };
            let location = info
                .location()
                .map(|loc| format!("{}:{}", loc.file(), loc.line()));
            let backtrace = Backtrace::force_capture();
            tracing::error!(panic = true, ?location, %panic_message, %backtrace, "stoatty panic");

            prior(info);
        }));
    });
}

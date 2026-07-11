//! Binary entry point for the `stoatty` terminal: parses argv, opens a window
//! running the requested command (or the user's shell), and drives the event
//! loop until the window closes.

use clap::{CommandFactory, Parser};
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

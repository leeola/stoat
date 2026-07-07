//! Binary entry point for the `stoatty` terminal: parses argv, opens a window
//! running the requested command (or the user's shell), and drives the event
//! loop until the window closes.

use clap::{CommandFactory, Parser};
use stoat_cli::FixtureSub;
use stoatty::cli::{Cli, TtyCommand};

fn main() {
    let mut cli = Cli::parse();

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

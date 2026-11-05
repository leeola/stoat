//! Binary entry point for the `stoatty` terminal: parses argv, opens a window
//! running the requested command (or the user's shell), and drives the event
//! loop until the window closes. The `shot` subcommand renders a script's shots
//! with no window instead.
//!
//! This is the process's syscall boundary. Logging is configured and its
//! directory created before anything else runs, so the environment and
//! filesystem reads here are the intended implementation.
#![allow(clippy::disallowed_methods)]

use clap::{CommandFactory, Parser};
use std::{backtrace::Backtrace, error::Error, panic, path::Path, slice, sync::Once};
use stoat_cli::FixtureSub;
use stoat_log::{
    ident::{self, LogId, ProcessIdent},
    LogTarget,
};
#[cfg(unix)]
use stoatty_bin::signals;
use stoatty_bin::{
    cli::{Adapter, Cli, ShotArgs, TtyCommand},
    config,
    headless::{
        script::{self, Step},
        HeadlessSession, Scene,
    },
    stoat_bin,
};

fn main() {
    // First, because a thread inherits the mask of the thread that spawns it,
    // and logging and the font scan start threads.
    #[cfg(unix)]
    if let Err(e) = signals::block_termination() {
        eprintln!("Failed to block the termination signals: {e}");
        std::process::exit(1);
    }

    let mut cli = Cli::parse();

    let id = LogId::mint();
    ident::install(ProcessIdent {
        file_stem: format!("stoatty-{id}"),
        id,
    });
    let installed = ident::get().expect("ident installed above");

    let stoat_log_env = std::env::var("STOAT_LOG").ok();
    let rust_log_env = std::env::var("RUST_LOG").ok();
    let target = match resolve_log_path(&installed.file_stem) {
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
    tracing::info!(
        log_id = %installed.id,
        commit = env!("STOATTY_BUILD_COMMIT"),
        commit_time = env!("STOATTY_BUILD_COMMIT_TIME"),
        built = env!("STOATTY_BUILD_TIME"),
        hostname = %ident::hostname(),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        cpus = std::thread::available_parallelism().map_or(0, |n| n.get()),
        "starting stoatty"
    );

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
        Some(TtyCommand::Shot(args)) => std::process::exit(run_shot(*args)),
        None => {},
    }

    stoatty_bin::app::run(
        cli.command(),
        cli.working_directory,
        cli.common,
        cli.terminal,
        cli.maximized,
    );
}

/// Render `args.script`'s shots with no window, and return the exit code.
///
/// Each failure exits 2 and is named on stderr: a script that does not parse,
/// no wgpu adapter, a child that fails to start or exits before the last step,
/// a termination signal, a settle that times out, and a shot not written.
fn run_shot(args: ShotArgs) -> i32 {
    match shoot(args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("stoatty shot: {}", error_chain(error.as_ref()));
            2
        },
    }
}

/// Play `args.script`, write each shot it names to `<out>/<name>.png`, and
/// print each path written on stdout.
fn shoot(args: ShotArgs) -> Result<(), Box<dyn Error>> {
    let steps = script::parse(&args.script)?;

    // The shipped config, never the user's, so two machines draw the same scene.
    let mut config = config::embedded_default();
    if let Some(theme) = &args.theme {
        config.theme.clone_from(theme);
    }

    let (program, program_args, stoat_dir) = match args.command() {
        Some((program, program_args)) => (program, program_args, None),
        None => {
            let stoat = stoat_bin::resolve(&config);
            let dir = stoat
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(Path::to_path_buf);
            (
                stoat.to_string_lossy().into_owned(),
                args.common.to_argv(),
                dir,
            )
        },
    };
    let working_directory = args.working_directory.and_then(|dir| {
        if dir.is_dir() {
            return Some(dir);
        }
        eprintln!(
            "stoatty shot: ignoring --working-dir {}: not a directory",
            dir.display()
        );
        None
    });

    // Bound before the session, so the session drops first and the child is
    // gone before its scratch home is removed.
    let scratch = match args.user_state {
        true => None,
        false => Some(tempfile::tempdir()?),
    };
    let extra_env = match &scratch {
        Some(home) => scratch_env(home.path())?,
        None => Vec::new(),
    };
    std::fs::create_dir_all(&args.out)?;

    let mut session = HeadlessSession::spawn(Scene {
        program,
        args: program_args,
        working_directory,
        stoat_dir,
        cols: args.size.cols,
        rows: args.size.rows,
        font_size: args.font_size.unwrap_or(config.font_size),
        terminal_font_size: config.terminal_font_size,
        scale_factor: args.scale,
        theme: config.resolve_theme(),
        theme_name: config.theme,
        font_family: config.font_family,
        ligatures: config.ligatures,
        cursor_animation: config.cursor_animation,
        software: args.adapter == Adapter::Software,
        extra_env,
    })?;
    #[cfg(unix)]
    session.forward_termination_signals()?;

    for step in &steps {
        let Step::Shot(name) = step else {
            session.type_keys(slice::from_ref(step))?;
            continue;
        };
        let shot = session.snapshot()?;
        let path = args.out.join(format!("{name}.png"));
        image::RgbaImage::from_raw(shot.width, shot.height, shot.rgba)
            .expect("the readback is width by height")
            .save(&path)?;
        println!("{}", path.display());
    }
    Ok(())
}

/// The XDG entries that point a child's config, data, and state at directories
/// made under `home`.
fn scratch_env(home: &Path) -> std::io::Result<Vec<(String, String)>> {
    [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
    ]
    .into_iter()
    .map(|(var, dir)| {
        let path = home.join(dir);
        std::fs::create_dir_all(&path)?;
        Ok((var.to_owned(), path.to_string_lossy().into_owned()))
    })
    .collect()
}

/// `error` and each error behind it, joined with ": ".
fn error_chain(error: &dyn Error) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        text.push_str(": ");
        text.push_str(&next.to_string());
        cause = next.source();
    }
    text
}

/// The log file for `stem` under the shared stoat log directory, at
/// `<log dir>/<stem>.log`. Creates the log directory if it does not yet exist.
///
/// `stem` carries the process's timestamped log id (e.g. `stoatty-<log-id>`), so
/// the filename sorts chronologically. It is the key that correlates this
/// session's files: an inner stoat and its LSP transcripts reuse the same id, so
/// they sort together in a directory listing.
fn resolve_log_path(stem: &str) -> std::io::Result<std::path::PathBuf> {
    let dir = stoat_log::log_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{stem}.log")))
}

/// Install a process-global panic hook that records the panic message,
/// location, and a captured backtrace via [`tracing::error`] before delegating
/// to the prior hook, so a panic survives in `stoatty-<log-id>.log` after the
/// window is gone. Idempotent across repeated calls.
///
/// A release build keeps its symbol table but not its debug info, so a captured
/// frame carries a function name and no file or line. The panic's own location
/// is logged separately and is exact either way. Build under the `profiling`
/// profile for frames that name a line too.
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

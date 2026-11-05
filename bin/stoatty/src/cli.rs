use clap::{Args, Parser, Subcommand, ValueEnum, ValueHint};
use std::path::PathBuf;
use stoat_cli::{CommonArgs, FixtureArgs};

/// The version string `stoatty --version` prints, and the value exported to
/// child programs via `STOATTY_VERSION` so an inner stoat can report it.
///
/// Shape is `<semver> (<hash>[-dirty] <date>)`. `STOATTY_BUILD_INFO` is the
/// git hash and date emitted by build.rs.
///
/// This is the diagnostic form, for a bug report that has to name the exact
/// build. A consumer comparing versions wants [`PACKAGE_VERSION`] instead,
/// since the spaces and parentheses here defeat an ordinary version parse.
pub(crate) const VERSION_INFO: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("STOATTY_BUILD_INFO"),
    ")",
);

/// The bare package version, exported to child programs via
/// `TERM_PROGRAM_VERSION`.
///
/// The ecosystem convention that variable belongs to exists so a program
/// decides what a terminal supports by comparing versions, so it carries a
/// plain semver rather than the build detail [`VERSION_INFO`] adds.
pub(crate) const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Per-invocation launch overrides for the stoatty terminal, parsed from argv.
///
/// Flags here apply to a single run and take precedence over the persistent
/// `[shell]` config; with no flags, stoatty falls back to the configured shell
/// or the system default.
#[derive(Parser)]
#[command(
    name = "stoatty",
    about = "A GPU-accelerated terminal emulator",
    version = VERSION_INFO
)]
pub struct Cli {
    #[command(subcommand)]
    pub command_sub: Option<TtyCommand>,

    /// Run PROGRAM (with any following arguments) instead of the shell, e.g.
    /// `stoatty -e nvim --clean`. Overrides the `[shell]` config for this run.
    #[arg(
        short = 'e',
        long = "command",
        value_name = "PROGRAM",
        allow_hyphen_values = true,
        num_args = 1..,
    )]
    command: Vec<String>,

    /// Set the spawned command's working directory for this run. Defaults to
    /// stoatty's own working directory when unset.
    #[arg(
        short = 'd',
        long = "working-dir",
        alias = "working-directory",
        value_name = "DIR",
        value_hint = ValueHint::DirPath
    )]
    pub working_directory: Option<PathBuf>,

    /// Files to open and the session-restore flags, forwarded to the stoat
    /// editor when it is the launched child. Ignored under `-e`/`--command` and
    /// `--terminal`, which run their own program.
    #[command(flatten)]
    pub common: CommonArgs,

    /// Run the login shell instead of the stoat editor, so stoatty opens as a
    /// plain terminal. Cannot combine with `-e`/`--command`, which already
    /// names its own program.
    #[arg(long = "terminal", conflicts_with = "command")]
    pub terminal: bool,

    /// Open the window maximized. The compositor decides the exact size, and
    /// the grid takes it from the first resize.
    #[arg(long = "maximized")]
    pub maximized: bool,
}

/// Subcommands stoatty handles before opening a window.
#[derive(Subcommand)]
pub enum TtyCommand {
    /// Materialize a deterministic fixture and open the editor inside it. `ls`
    /// lists the catalog. Forwarded to the stoat child.
    Fixture(FixtureArgs),

    /// Print a shell completion script to stdout, e.g. `stoatty completions fish >
    /// ~/.config/fish/completions/stoatty.fish` (zsh and bash install the same
    /// way).
    Completions {
        /// The shell to generate completions for.
        shell: clap_complete::Shell,
    },

    /// Render a script's shots headlessly to PNG files, with no window.
    Shot(Box<ShotArgs>),
}

/// What `stoatty shot` runs, the script it plays, and where its shots go.
///
/// A run draws from the shipped config, never the user's, so two machines draw
/// the same scene.
#[derive(Args)]
pub struct ShotArgs {
    /// Files to open and the session-restore flags, forwarded to the stoat
    /// editor when it is the child. Ignored under `-e`/`--command`.
    #[command(flatten)]
    pub common: CommonArgs,

    /// Run PROGRAM (with any following arguments) instead of the stoat editor.
    /// Every argument after it goes to PROGRAM, so give it last.
    #[arg(
        short = 'e',
        long = "command",
        value_name = "PROGRAM",
        allow_hyphen_values = true,
        num_args = 1..,
    )]
    command: Vec<String>,

    /// The child's working directory. Defaults to stoatty's own.
    #[arg(
        short = 'd',
        long = "working-dir",
        value_name = "DIR",
        value_hint = ValueHint::DirPath
    )]
    pub working_directory: Option<PathBuf>,

    /// The terminal grid, in cells.
    #[arg(
        long,
        value_name = "COLSxROWS",
        default_value = "120x40",
        value_parser = parse_grid_size
    )]
    pub size: GridSize,

    /// The editor font size in points. Defaults to the shipped config's.
    #[arg(long)]
    pub font_size: Option<u32>,

    /// Physical pixels per point.
    #[arg(long, default_value_t = 1.0)]
    pub scale: f64,

    /// The theme to draw with. Defaults to the shipped config's.
    #[arg(long)]
    pub theme: Option<String>,

    /// The GPU adapter that draws.
    #[arg(long, value_enum, default_value_t = Adapter::Software)]
    pub adapter: Adapter,

    /// The key script to play, in the grammar `stoat --inputs` reads, with
    /// `<Settle>` and `<Shot:name>` added.
    #[arg(long, default_value = "<Settle><Shot:final>")]
    pub script: String,

    /// The directory each `<Shot:name>` writes `name.png` into, made when it
    /// does not exist.
    #[arg(long, value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub out: PathBuf,

    /// Give the child the user's own config, data, and state directories,
    /// instead of a scratch home that the run removes.
    #[arg(long)]
    pub user_state: bool,
}

/// A terminal grid size in cells, parsed from `COLSxROWS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

/// The GPU adapter a headless run draws on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Adapter {
    /// A CPU adapter such as lavapipe, which draws the same pixels on every
    /// machine.
    Software,
    /// The machine's own GPU.
    Hardware,
}

impl Cli {
    /// The launch program and its arguments from `-e`/`--command`, or `None`
    /// when the flag was not given. The first token is the program, the rest
    /// its arguments.
    pub fn command(&self) -> Option<(String, Vec<String>)> {
        split_command(&self.command)
    }
}

impl ShotArgs {
    /// The program and its arguments from `-e`/`--command`, or `None` when the
    /// flag was not given, as [`Cli::command`] reads them.
    pub fn command(&self) -> Option<(String, Vec<String>)> {
        split_command(&self.command)
    }
}

/// The program and its arguments from the tokens of a `-e`/`--command` flag.
fn split_command(command: &[String]) -> Option<(String, Vec<String>)> {
    let (program, args) = command.split_first()?;
    Some((program.clone(), args.to_vec()))
}

/// Parse a `COLSxROWS` grid size, such as `120x40`.
///
/// Each side is a whole number of cells above zero, since a grid with no cells
/// has no texture to draw into.
fn parse_grid_size(text: &str) -> Result<GridSize, String> {
    let side = |side: &str| side.parse::<u16>().ok().filter(|&cells| cells > 0);
    match text
        .split_once(['x', 'X'])
        .map(|(cols, rows)| (side(cols), side(rows)))
    {
        Some((Some(cols), Some(rows))) => Ok(GridSize { cols, rows }),
        _ => Err(format!(
            "expected COLSxROWS with whole numbers above zero, such as 120x40, not '{text}'"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn command_splits_program_from_args() {
        let cli = Cli::parse_from(["stoatty", "-e", "nvim", "--clean"]);
        assert_eq!(
            cli.command(),
            Some(("nvim".to_string(), vec!["--clean".to_string()]))
        );
    }

    #[test]
    fn bare_argv_has_no_command() {
        let cli = Cli::parse_from(["stoatty"]);
        assert_eq!(cli.command(), None);
    }

    #[test]
    fn command_without_args() {
        let cli = Cli::parse_from(["stoatty", "--command", "ls"]);
        assert_eq!(cli.command(), Some(("ls".to_string(), Vec::new())));
    }

    #[test]
    fn working_directory_parses_path() {
        use std::path::PathBuf;

        for flag in ["--working-dir", "-d", "--working-directory"] {
            let cli = Cli::parse_from(["stoatty", flag, "/tmp"]);
            assert_eq!(
                cli.working_directory,
                Some(PathBuf::from("/tmp")),
                "{flag} parses the working directory",
            );
        }
        assert_eq!(Cli::parse_from(["stoatty"]).working_directory, None);
    }

    #[test]
    fn bare_positionals_collect_as_files() {
        use std::path::PathBuf;

        let cli = Cli::parse_from(["stoatty", "a.rs", "b.rs"]);
        assert_eq!(
            cli.common.files,
            vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]
        );
        assert_eq!(cli.command(), None);
    }

    #[test]
    fn dash_e_consumes_trailing_args_leaving_files_empty() {
        use std::path::PathBuf;

        let cli = Cli::parse_from(["stoatty", "-e", "nvim", "a.rs"]);
        assert_eq!(
            cli.command(),
            Some(("nvim".to_string(), vec!["a.rs".to_string()]))
        );
        assert_eq!(cli.common.files, Vec::<PathBuf>::new());
    }

    #[test]
    fn terminal_flag_parses() {
        assert!(Cli::parse_from(["stoatty", "--terminal"]).terminal);
        assert!(!Cli::parse_from(["stoatty"]).terminal);
    }

    #[test]
    fn maximized_flag_parses() {
        assert!(Cli::parse_from(["stoatty", "--maximized"]).maximized);
        assert!(!Cli::parse_from(["stoatty"]).maximized);
    }

    #[test]
    fn terminal_conflicts_with_command() {
        assert!(Cli::try_parse_from(["stoatty", "--terminal", "-e", "sh"]).is_err());
    }

    #[test]
    fn fixture_subcommand_parses_ls_and_a_name() {
        use stoat_cli::FixtureSub;

        let cli = Cli::parse_from(["stoatty", "fixture", "ls"]);
        let Some(super::TtyCommand::Fixture(args)) = cli.command_sub else {
            panic!("expected the fixture subcommand");
        };
        assert_eq!(args.sub, Some(FixtureSub::Ls));
        assert_eq!(args.name, None);

        let cli = Cli::parse_from(["stoatty", "fixture", "conflict"]);
        let Some(super::TtyCommand::Fixture(args)) = cli.command_sub else {
            panic!("expected the fixture subcommand");
        };
        assert_eq!(args.sub, None);
        assert_eq!(args.name.as_deref(), Some("conflict"));
    }

    #[test]
    fn fixture_flag_parses_without_a_subcommand() {
        let cli = Cli::parse_from(["stoatty", "--fixture", "basic-diff"]);
        assert!(cli.command_sub.is_none());
        assert_eq!(cli.common.fixture.as_deref(), Some("basic-diff"));
    }

    #[test]
    fn completions_carry_the_fixture_names() {
        use clap::CommandFactory;

        let mut out = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Fish,
            &mut <Cli as CommandFactory>::command(),
            "stoatty",
            &mut out,
        );
        let script = String::from_utf8(out).expect("fish script is utf8");
        assert!(
            script.contains("rust-lsp"),
            "generated completions must offer the fixture names"
        );
    }

    #[test]
    fn shot_parses_size_script_and_out() {
        use super::{Adapter, GridSize, ShotArgs, TtyCommand};
        use std::path::PathBuf;

        let shot = |argv: &[&str]| -> ShotArgs {
            match Cli::parse_from(["stoatty", "shot"].iter().chain(argv)).command_sub {
                Some(TtyCommand::Shot(args)) => *args,
                _ => panic!("expected the shot subcommand"),
            }
        };

        let given = shot(&[
            "--size",
            "80x24",
            "--script",
            "jj<Shot:a>",
            "--out",
            "shots",
            "--adapter",
            "hardware",
            "--user-state",
            "-e",
            "sh",
            "-c",
            "true",
        ]);
        assert_eq!(
            (
                given.size,
                given.script.as_str(),
                given.out.clone(),
                given.adapter,
                given.user_state,
                given.command(),
            ),
            (
                GridSize { cols: 80, rows: 24 },
                "jj<Shot:a>",
                PathBuf::from("shots"),
                Adapter::Hardware,
                true,
                Some(("sh".to_owned(), vec!["-c".to_owned(), "true".to_owned()])),
            ),
            "every flag as given, with -e last taking the rest",
        );

        let defaults = shot(&["--out", "shots"]);
        assert_eq!(
            (
                defaults.size,
                defaults.script.as_str(),
                defaults.adapter,
                defaults.scale,
                defaults.user_state,
                defaults.command(),
            ),
            (
                GridSize {
                    cols: 120,
                    rows: 40
                },
                "<Settle><Shot:final>",
                Adapter::Software,
                1.0,
                false,
                None,
            ),
            "the defaults when only --out is given",
        );
    }

    #[test]
    fn shot_rejects_a_malformed_size() {
        use clap::error::ErrorKind;

        let kinds: Vec<_> = ["80", "80x", "x24", "0x24", "80x0", "80x24x2", "wide"]
            .into_iter()
            .map(|size| {
                Cli::try_parse_from(["stoatty", "shot", "--out", "shots", "--size", size])
                    .err()
                    .map(|error| error.kind())
            })
            .collect();
        assert_eq!(kinds, [Some(ErrorKind::ValueValidation); 7]);
    }
}

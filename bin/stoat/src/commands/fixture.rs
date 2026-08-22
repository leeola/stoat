//! The `stoat fixture` subcommand's `ls` handler.

/// Print the fixture catalog to stdout, one `name  description` line per entry.
///
/// The catalog is just strings, so this works in any build, independent of the
/// `fixture` feature that gates actually materializing one.
pub fn run_ls() {
    print!("{}", stoat_cli::ls_text());
}

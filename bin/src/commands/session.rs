use crate::commands::client;
use clap::Subcommand;
use snafu::Whatever;

#[derive(Subcommand, Debug)]
pub enum SessionCommand {
    /// List live sessions: id, root directory, and window and buffer counts.
    List,
}

pub fn run(sub: SessionCommand) -> Result<(), Whatever> {
    match sub {
        SessionCommand::List => list(),
    }
}

fn list() -> Result<(), Whatever> {
    let sessions = client::list_sessions_from_app()?;
    if sessions.is_empty() {
        println!("No live sessions.");
        return Ok(());
    }

    let id_width = sessions
        .iter()
        .map(|s| s.id.to_string().len())
        .max()
        .unwrap_or(0)
        .max(2);
    let root_width = sessions
        .iter()
        .map(|s| s.root.len())
        .max()
        .unwrap_or(0)
        .max(4);

    println!(
        "{:<id_width$}  {:<root_width$}  {:>7}  {:>7}",
        "ID", "ROOT", "WINDOWS", "BUFFERS"
    );
    for s in &sessions {
        println!(
            "{:<id_width$}  {:<root_width$}  {:>7}  {:>7}",
            s.id, s.root, s.windows, s.buffers,
        );
    }
    Ok(())
}

use crate::commands::client::{self, BufferRowInfo};
use clap::Subcommand;
use snafu::Whatever;

#[derive(Subcommand, Debug)]
pub enum SessionCommand {
    /// List live sessions: id, root directory, and window and buffer counts.
    List,
    /// List a session's buffers: id, path or scratch label, and dirty flag.
    Buffers {
        /// The session's WorkspaceUid, as shown by `session list`.
        id: u64,
    },
    /// Close a live session: persist it, then drop it and its window(s).
    Close {
        /// The session's WorkspaceUid, as shown by `session list`.
        id: u64,
    },
}

pub fn run(sub: SessionCommand) -> Result<(), Whatever> {
    match sub {
        SessionCommand::List => list(),
        SessionCommand::Buffers { id } => buffers(id),
        SessionCommand::Close { id } => close(id),
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

fn buffers(id: u64) -> Result<(), Whatever> {
    let buffers = client::list_buffers_from_app(id)?;
    if buffers.is_empty() {
        println!("No buffers in session {id}.");
        return Ok(());
    }

    let id_width = buffers
        .iter()
        .map(|b| b.id.to_string().len())
        .max()
        .unwrap_or(0)
        .max(2);
    let path_width = buffers
        .iter()
        .map(|b| label(b).len())
        .max()
        .unwrap_or(0)
        .max(4);

    println!("{:<id_width$}  {:<path_width$}  DIRTY", "ID", "PATH");
    for b in &buffers {
        println!(
            "{:<id_width$}  {:<path_width$}  {}",
            b.id,
            label(b),
            if b.dirty { "yes" } else { "no" },
        );
    }
    Ok(())
}

/// The display label for a buffer: its path, or `(scratch)` when it has none.
fn label(buffer: &BufferRowInfo) -> &str {
    buffer.path.as_deref().unwrap_or("(scratch)")
}

fn close(id: u64) -> Result<(), Whatever> {
    client::close_session_in_app(id)?;
    println!("Closed session {id}.");
    Ok(())
}
